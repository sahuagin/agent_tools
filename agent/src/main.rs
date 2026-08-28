mod adjudicate;
mod db;
mod dialogue;
mod dialogue_presence;
mod embed;
mod forward;
mod kx;
mod mcp;
mod memory;
mod metrics;
mod tasks;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agent",
    about = "Agent memory, tasks, and metrics store",
    version = build_version(),
    after_long_help = ROOT_AFTER_LONG_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// at-g5u: the version string includes build provenance (change id + build
/// time) so "is this binary current with source?" is one command. The
/// deployed `~/.local/bin/agent` is a symlink into the checkout's
/// `target/release/agent`, so "deployed" silently tracks whatever was last
/// built there — the provenance makes staleness checkable instead of
/// guessable.
fn build_version() -> &'static str {
    // Baked in by build.rs (AGENT_CHANGE_ID, AGENT_BUILD_TIME, AGENT_DIRTY).
    // Fallback to the Cargo.toml version if the env vars aren't set
    // (e.g. a stale binary built before at-g5u).
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        let change_id = std::option_env!("AGENT_CHANGE_ID").unwrap_or("unknown");
        let build_time = std::option_env!("AGENT_BUILD_TIME").unwrap_or("unknown");
        let dirty = std::option_env!("AGENT_DIRTY").unwrap_or("unknown");
        format!("0.1.0 ({change_id}, {build_time}, {dirty})")
    })
    .as_str()
}

#[derive(Subcommand)]
enum Command {
    /// Memory store: add, update, search, recall, context, migrate
    Memory(memory::MemoryCmd),
    /// Task state machine: create, update, list, show, resume
    Task(tasks::TaskCmd),
    /// Metrics: record-completion, record-usage, report, list
    Metrics(metrics::MetricsCmd),
    /// Inter-agent dialogue over the mu-dialogue MCP: watch, poll, say, peers
    Dialogue(dialogue::DialogueCmd),
    /// Knowledge index: ingest a document corpus, then recall/search/list it
    Kx(kx::KxCmd),
    /// Print path to agent.sqlite
    DbPath,
}

fn main() -> Result<()> {
    // Status + diagnostics via `log`/env_logger (stderr): confirmations
    // at info (visible by default, RUST_LOG=warn silences), debug/trace
    // compiled out of release builds (release_max_level_info). Command
    // RESULTS (listings, --json) stay on stdout — the tool contract.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Agent-oriented help: `agent [<group> [<sub>]] --help-ai [--json]`.
    // Handled before clap so it works even when the target command's required
    // args are absent (mirrors code-index's --help-ai).
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--help-ai") {
        print_ai_help(&argv, argv.iter().any(|a| a == "--json"));
        return Ok(());
    }

    // Client mode: forward to a remote agent-mcp instead of the local DB when a
    // target resolves — `--via-mcp <url>` flag, `AGENT_MCP_URL`, or the
    // config-default `[agent] mcp_url`. Handled before clap so the local
    // DB/embedder are never opened in client mode. If the endpoint is unreachable
    // for a read-only verb, fall back to the local DB (writes fail loud inside
    // `forward::run`).
    if let Some((url, rest)) = forward::target(&argv) {
        match forward::run(&url, &rest)? {
            forward::Outcome::Done => return Ok(()),
            forward::Outcome::FallBackLocal => {
                // Re-parse the stripped argv (no --via-mcp) for the local path.
                return dispatch_local(Cli::parse_from(
                    std::iter::once("agent".to_string()).chain(rest),
                ));
            }
        }
    }

    dispatch_local(Cli::parse())
}

/// Dispatch a parsed command against the LOCAL SQLite DB (the non-forwarded
/// path). DbPath and Dialogue never open the DB — DbPath just prints the path,
/// Dialogue talks to a remote mu-dialogue MCP like a thin client.
fn dispatch_local(cli: Cli) -> Result<()> {
    match cli.command {
        Command::DbPath => {
            println!("{}", db::db_path().display());
            Ok(())
        }
        Command::Dialogue(cmd) => dialogue::run(cmd),
        Command::Memory(cmd) => memory::run(db::open()?, cmd),
        Command::Task(cmd) => tasks::run(db::open()?, cmd),
        Command::Metrics(cmd) => metrics::run(db::open()?, cmd),
        Command::Kx(cmd) => kx::run(db::open()?, cmd),
    }
}

// ── Agent-oriented help (--help-ai) ──────────────────────────────────────────
// Terse, structured help meant for an agent driving the CLI: documents inputs,
// output shape, and when-to-use rules the human --help leaves implicit.
//
// Plain text (`ai_text`) by default. `--json` emits a STRUCTURED command spec
// introspected from the live clap tree — every group, subcommand, arg, flag,
// value-name, default, and possible-value, recursively — scoped to the
// requested path. Because it walks the real `Command`, it cannot drift from
// the actual surface the way the hand-written prose does; this is the catalog
// an rmcp layer reads to advertise each leaf as a tool. The curated prose for
// the scoped node rides along under `help_ai`.

fn print_ai_help(argv: &[String], json: bool) {
    let positional: Vec<&str> = argv
        .iter()
        .map(String::as_str)
        .filter(|s| !s.starts_with('-'))
        .collect();
    let group = positional.first().copied().unwrap_or("");
    let sub = positional.get(1).copied().unwrap_or("");
    let text = ai_text(group, sub);
    if json {
        // Structural catalog comes from clap-catalog (the reusable generator);
        // agent attaches its curated per-node prose under `help_ai`.
        let mut spec = clap_catalog::catalog_scoped::<Cli>(&positional);
        if let serde_json::Value::Object(map) = &mut spec {
            map.insert(
                "help_ai".into(),
                serde_json::Value::String(text.to_string()),
            );
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&spec).unwrap_or_else(|_| spec.to_string())
        );
    } else {
        println!("{text}");
    }
}

fn ai_text(group: &str, sub: &str) -> &'static str {
    match (group, sub) {
        ("memory", "add") => MEM_ADD_AI,
        ("memory", "update") => MEM_UPDATE_AI,
        ("memory", "search") => MEM_SEARCH_AI,
        ("memory", "recall") => MEM_RECALL_AI,
        ("memory", "recent") => MEM_RECENT_AI,
        ("memory", "list") => MEM_LIST_AI,
        ("memory", "context") => MEM_CONTEXT_AI,
        ("memory", "context-stats") => MEM_CONTEXT_STATS_AI,
        ("memory", "rebuild-index") => MEM_REBUILD_AI,
        ("memory", "reindex") => MEM_REINDEX_AI,
        ("memory", "export") => MEM_EXPORT_AI,
        ("memory", "migrate") => MEM_MIGRATE_AI,
        ("memory", _) => MEM_OVERVIEW_AI,
        ("task", "create") => TASK_CREATE_AI,
        ("task", "update") => TASK_UPDATE_AI,
        ("task", "list") => TASK_LIST_AI,
        ("task", "show") => TASK_SHOW_AI,
        ("task", "resume") => TASK_RESUME_AI,
        ("task", _) => TASK_OVERVIEW_AI,
        ("metrics", "record-completion") => METRICS_RECORD_COMPLETION_AI,
        ("metrics", "record-usage") => METRICS_RECORD_USAGE_AI,
        ("metrics", "report") => METRICS_REPORT_AI,
        ("metrics", "list") => METRICS_LIST_AI,
        ("metrics", _) => METRICS_OVERVIEW_AI,
        ("dialogue", _) => DIALOGUE_AI,
        ("kx", _) => KX_AI,
        ("db-path", _) => DBPATH_AI,
        _ => ROOT_AI,
    }
}

const ROOT_AFTER_LONG_HELP: &str = "\
AGENT-ORIENTED HELP:
  agent --help-ai                  # terse structured overview (for agents)
  agent --help-ai --json           # same, as JSON
  agent memory recall --help-ai    # per-command detail (output, flags, when-to-use)

COMMON:
  agent memory context --cwd \"$PWD\"                 # session-start memory load
  agent memory add --type feedback --name x --description y --content-file /tmp/body
  agent memory recall \"how do we X\" --k 6 --full";

const ROOT_AI: &str = "\
# agent — memory / tasks / metrics store (agent-oriented help)

PURPOSE
  Local SQLite store (~/.local/share/agent.sqlite) for cross-session agent
  memory, a task state machine, and run metrics. Shared across all claude
  accounts and pi sub-agents.

GROUPS
  memory   add/update/search/recall/recent/list/context/context-stats/
           rebuild-index/reindex/export/migrate
  task     create/update/list/show/resume
  metrics  record-completion/record-usage/report/list
  dialogue watch/poll/say/peers — inter-agent mailbox (mu-dialogue MCP)
  kx       ingest/recall/search/list — document knowledge index
  db-path  print path to agent.sqlite

DETAIL
  agent memory --help-ai           # group overview
  agent memory recall --help-ai    # per-command detail
  add --json to any of the above for JSON.";

const MEM_OVERVIEW_AI: &str = "\
# agent memory — cross-session memory store (agent-oriented help)

WHEN TO USE WHICH
  context  : session start — auto-loads relevant memories (hook use).
  recall   : prose / behavioral (\"how do we X\") — embedding similarity.
  search   : exact keyword / symbol — FTS5 lexical.
  add      : save a new memory (types: user|feedback|project|reference).
  update   : edit fields of an existing memory by id.
  recent / list : browse by recency / by type|tag|cwd.

WRITING CONTENT (add/update)
  --content <text> | --content - (stdin) | --content-file <path>.
  Prefer --content-file for large/free-text bodies: no shell-quoting, and the
  body never lands on the command line (avoids command guards). Precedence:
  --content-file > --content - > --content.

DETAIL: agent memory <cmd> --help-ai [--json]";

const MEM_ADD_AI: &str = "\
# agent memory add — save a new memory

USAGE
  agent memory add --type <user|feedback|project|reference> --name <slug> --description <hook> --content-file <path> [--tags t1,t2] [--cwd PATH] [--source curated]

CONTENT INPUT (one required; precedence)
  --content-file <path>  >  --content -  (stdin)  >  --content <inline>
  Use --content-file or stdin for anything with quotes, newlines, $, backticks,
  or strings a command guard might flag (e.g. rm -rf, find -delete).

OUTPUT
  The new memory id (8 hex chars) on stdout.

NOTES
  Embeds the memory for semantic recall (OPENROUTER_API_KEY, config.toml
  fallback). type must be one of the four; tags are comma-separated.";

const MEM_UPDATE_AI: &str = "\
# agent memory update — edit an existing memory

USAGE
  agent memory update <id> [--name ..] [--description ..] [--content <text>|--content -|--content-file <path>] [--tags ..] [--active true|false]

NOTES
  Only the fields you pass change. Content input precedence matches add
  (--content-file > stdin > inline). Re-embeds from the post-update state.
  --active false soft-hides a memory (kept, excluded from recall/context).
  Errors if no memory matches <id>.";

const MEM_SEARCH_AI: &str = "\
# agent memory search — FTS5 lexical search

USAGE
  agent memory search \"<query>\" [--type T] [--limit N=10]

WHEN
  Exact keywords / symbols. For prose or behavioral queries use `recall`.

OUTPUT
  Matching memories (id, type, name, description), ranked by FTS5 BM25.";

const MEM_RECALL_AI: &str = "\
# agent memory recall — semantic recall via embeddings

USAGE
  agent memory recall \"<query>\" [--k N=5] [--type T] [--compare] [--full] [--json]

WHEN
  Prose / behavioral (\"how do we X\", \"what did we decide about Y\"). Embedding
  cosine similarity. Needs OPENROUTER_API_KEY (config.toml fallback).

FLAGS
  --k N       number of results          --full     include full content body
  --type T    restrict to a type         --compare  also show FTS side-by-side
  --json      machine-readable output

OUTPUT
  Ranked rows: [score] [id] (type) name — description; full body when --full.";

const MEM_RECENT_AI: &str = "\
# agent memory recent — most recently updated memories

USAGE
  agent memory recent [--n N=10] [--type T]

OUTPUT
  Recent memories (id, type, name, description), newest first.";

const MEM_LIST_AI: &str = "\
# agent memory list — list memories by filter

USAGE
  agent memory list [--type T] [--tag TAG] [--cwd PATH] [--limit N]

WHEN
  Browse / enumerate by category. For relevance ranking use recall or search.";

const MEM_CONTEXT_AI: &str = "\
# agent memory context — session-start memory load (hook use)

USAGE
  agent memory context [--cwd \"$PWD\"]

PURPOSE
  Emits the relevant-memory block injected at session start (the SessionStart
  hook calls this). Scores by cwd + type + recency. Run manually to re-load
  context after a compaction.";

const MEM_CONTEXT_STATS_AI: &str = "\
# agent memory context-stats — context-call log (tuning)

USAGE
  agent memory context-stats [--n N]

PURPOSE
  Recent `context` invocations and what they surfaced — for tuning retrieval.";

const MEM_REBUILD_AI: &str = "\
# agent memory rebuild-index — rebuild the topic index

USAGE
  agent memory rebuild-index

PURPOSE
  Rebuilds the FTS/topic index for all active memories. Maintenance op; run
  after bulk imports or if search results look stale.";

const MEM_REINDEX_AI: &str = "\
# agent memory reindex — (re-)embed memories

USAGE
  agent memory reindex [--missing] [--batch N=16]

PURPOSE
  Computes embeddings. --missing embeds only memories lacking an embedding for
  the active model (use after a model/dims change). Needs the embedder
  configured (OPENROUTER_API_KEY).";

const MEM_EXPORT_AI: &str = "\
# agent memory export — dump active memories as markdown

USAGE
  agent memory export

OUTPUT
  All active memories rendered as markdown on stdout (backup / review).";

const MEM_MIGRATE_AI: &str = "\
# agent memory migrate — import markdown memory files

USAGE
  agent memory migrate --dir <path> [--dry-run]

PURPOSE
  One-time import of legacy markdown memory files into the DB. --dry-run prints
  what would be imported without writing.";

const TASK_OVERVIEW_AI: &str = "\
# agent task — task state machine (agent-oriented help)

ACTIONS
  create  start a task (objective, type, cwd, optional parent)
  update  set status/result/completion-id on a task by id
  list    recent tasks (filter by status/cwd/days)
  show    full details of one task
  resume  list in_progress + suspended tasks to pick up

DETAIL: agent task <cmd> --help-ai [--json]";

const TASK_CREATE_AI: &str = "\
# agent task create — start a task

USAGE
  agent task create --objective \"<what>\" [--task-type research] [--cwd PATH] [--parent-id ID]

OUTPUT
  The new task id on stdout.";

const TASK_UPDATE_AI: &str = "\
# agent task update — update a task

USAGE
  agent task update <id> [--status S] [--result \"<text>\"] [--completion-id ID]

NOTES
  status is free-form (e.g. in_progress, completed, suspended). Only the fields
  you pass change.";

const TASK_LIST_AI: &str = "\
# agent task list — list tasks

USAGE
  agent task list [--status S] [--cwd PATH] [--limit N=20] [--days N=7]";

const TASK_SHOW_AI: &str = "\
# agent task show — full task details

USAGE
  agent task show <id>";

const TASK_RESUME_AI: &str = "\
# agent task resume — resumable tasks

USAGE
  agent task resume

PURPOSE
  Lists in_progress and suspended tasks so a session can pick up unfinished work.";

const METRICS_OVERVIEW_AI: &str = "\
# agent metrics — run metrics (agent-oriented help)

ACTIONS
  record-completion  one row per orchestrate run / pi session
  record-usage       token/cost usage tied to a completion
  report             aggregated metrics
  list               recent completions

DETAIL: agent metrics <cmd> --help-ai [--json]";

const METRICS_RECORD_COMPLETION_AI: &str = "\
# agent metrics record-completion — record a completion

USAGE
  agent metrics record-completion [--task-id ID] [--session-id ID] [--model M] [--provider P] [--task-type T] [--objective ..] [--cwd PATH]

OUTPUT
  The completion id on stdout (pass to record-usage).";

const METRICS_RECORD_USAGE_AI: &str = "\
# agent metrics record-usage — record token/cost usage

USAGE
  agent metrics record-usage --completion-id <ID> [token/cost flags]

PURPOSE
  Attaches token and cost usage to a completion row created by
  record-completion. See `agent metrics record-usage --help` for exact flags.";

const METRICS_REPORT_AI: &str = "\
# agent metrics report — aggregated metrics

USAGE
  agent metrics report [filters]

OUTPUT
  Aggregated completion/usage stats (by model/provider/type as available).
  See `agent metrics report --help` for filters.";

const METRICS_LIST_AI: &str = "\
# agent metrics list — recent completions

USAGE
  agent metrics list [--limit N]";

const DIALOGUE_AI: &str = "\
# agent dialogue — inter-agent mailbox over mu-dialogue MCP (agent-oriented help)

ACTIONS
  watch  long-poll for new messages to <id>; print one line per NEW message
         (built to run under a Monitor — wakes the agent only when a peer writes)
  poll   one-shot poll; print the raw {messages:[...]} JSON
  say    send a message (--from --to --content [--thread])
  peers  list peers on the channel

USAGE
  agent dialogue watch cc:$CLAUDE_CODE_SESSION_ID
  agent dialogue say --from cc:<me> --to cc:<peer> --content \"...\"

NOTES
  Endpoint: --url, else AGENT_DIALOGUE_URL, else the deployed server. A thin
  sync MCP client (ureq); no local DB. watch keeps a `since` watermark so each
  message surfaces once.";

const KX_AI: &str = "\
# agent kx — document knowledge index (agent-oriented help)

WHAT IT IS
  A kx entry is a POINTER (+ one-line summary + metadata + embedding) to one
  document in a configured corpus; the files stay on disk. Stored as memories
  with type='kx', kept OUT of personal-memory recall/context.

ACTIONS
  ingest  scan the configured [[kx.sources]] and index new/changed docs
          (no args = all sources; --source, --dry-run, --force, --config)
  recall  SEMANTIC recall over the corpus (embeddings + cosine, honors min_score)
  search  FTS5 lexical search over the corpus
  list    browse by tag / scope / recency

FILTERS (recall/search/list)
  --scope S   ownership scope (work/personal/shared)
  --tag T     require tag (repeatable; ANDed)   --from/--to YYYY-MM-DD (by doc date)
  --k / --limit / --min-score   caps and the recall cosine floor
  --json      machine-readable output

CONFIG
  [kx] in ~/.config/agent/config.toml: global knobs + [[kx.sources]] corpora.
  See config.toml.example. Never logs the file's API keys.

DETAIL: agent kx <cmd> --help-ai [--json]";

const DBPATH_AI: &str = "\
# agent db-path — print the store path

USAGE
  agent db-path

OUTPUT
  Absolute path to agent.sqlite.";
