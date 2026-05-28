//! `code-index` CLI entry point.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use code_index::edges::build_edges;
use code_index::embed::{embed_pending_concurrent, select_embedder};
use code_index::graph::Graph;
use code_index::ingest::ingest_with;
use code_index::recall::{recall_tuned, RecallMode, RecallTuning, DEFAULT_TEST_PENALTY};
use code_index::store::SqliteStore;
use code_index::{ChunkId, Store};

#[derive(Parser, Debug)]
#[command(
    name = "code-index",
    version,
    about = "Code-aware indexing and retrieval for agentic workflows.",
    after_long_help = TOP_AFTER_LONG_HELP
)]
struct Cli {
    /// Path to the index database. If omitted, code-index walks up from
    /// the current directory looking for a `.code_index/index.db`; if not
    /// found, falls back to `~/.cache/code_index/<basename-of-cwd>.db`.
    /// Set `CODE_INDEX_DB` env var to override the discovery without
    /// passing `--db` on every invocation.
    #[arg(long, global = true)]
    db: Option<std::path::PathBuf>,

    /// Emit machine-readable, agent-oriented help for the (sub)command and
    /// exit. Documents output schemas, sentinels, and when-to-use rules
    /// that the human help leaves implicit. Combine with `--json` for a
    /// structured object instead of terse text.
    #[arg(long, global = true)]
    help_ai: bool,

    /// Emit JSON instead of text. Currently only modifies `--help-ai`
    /// output; reserved for structured command output in future.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Walk a path, chunk via tree-sitter, embed, and persist to the store.
    #[command(after_long_help = INGEST_AFTER_LONG_HELP)]
    Ingest {
        /// Path to walk. Optional only so `--help-ai` works without one;
        /// required for an actual ingest (use `.` for the current dir).
        path: Option<std::path::PathBuf>,
        /// Skip embedding pass — useful for offline indexing or when you
        /// just want chunk metadata in the DB.
        #[arg(long)]
        no_embed: bool,
        /// Embedding batch size. Lower values reduce blast radius when one
        /// batch trips an upstream (you find the offending chunk faster);
        /// higher values amortize HTTP overhead.
        #[arg(long, default_value_t = 16)]
        embed_batch_size: usize,
        /// In-flight HTTP request concurrency for embedding. Each worker
        /// blocks on its own socket; the kernel parks them in `sbwait`
        /// independently. Real wall-clock scales ~linearly until you hit
        /// the upstream's rate limit. Set to 1 to disable concurrency.
        #[arg(long, default_value_t = 8)]
        embed_concurrency: usize,
        /// Skip the project's `.gitignore` / `.ignore` / `.git/info/exclude`
        /// rules when walking. Useful when you want to index ignored paths
        /// (e.g. a vendored dependency you do want searchable).
        #[arg(long)]
        no_gitignore: bool,
    },
    /// Recall over indexed chunks. Returns ranked (id, score) pairs.
    /// Combines semantic embedding similarity with lexical FTS5 BM25
    /// by default — toggle via `--mode`.
    #[command(after_long_help = RECALL_AFTER_LONG_HELP)]
    Recall {
        /// Query: a prose description of behavior (semantic/hybrid) or an
        /// exact symbol/string (lexical). Optional only so `--help-ai`
        /// works without one; required for an actual recall.
        query: Option<String>,
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// Materialize and print chunk contents for results.
        #[arg(long)]
        full: bool,
        /// Recall strategy: `hybrid` (default, semantic + lexical via RRF),
        /// `semantic` (embedding cosine only), or `lexical` (FTS5 BM25 only).
        #[arg(long, default_value = "hybrid")]
        mode: String,
        /// Disable the default down-weight on test chunks. Tests
        /// over-rank for natural-language queries because their function
        /// names tend to be descriptive prose; default behavior applies a
        /// 0.5x multiplier to test scores so source code surfaces ahead
        /// of equivalent test matches. Pass `--no-test-penalty` to opt
        /// out (e.g. when you ARE looking for tests).
        #[arg(long)]
        no_test_penalty: bool,
        /// Drop test chunks from results entirely. Stronger than
        /// `--no-test-penalty`'s opposite — even with the penalty active,
        /// tests can show up among low-scoring tail; this filter removes
        /// them outright.
        #[arg(long)]
        exclude_tests: bool,
    },
    /// Graph operations — build edges, run analyzers, inspect communities.
    #[command(after_long_help = GRAPH_AFTER_LONG_HELP)]
    Graph {
        /// Optional only so `code-index graph --help-ai` works without an
        /// op; required for an actual graph operation.
        #[command(subcommand)]
        op: Option<GraphOp>,
    },
    /// What's indexed, when, how big. Prints DB path, file count, chunk
    /// distribution by kind, embeddings by model, edge distribution.
    #[command(after_long_help = STATUS_AFTER_LONG_HELP)]
    Status,
    /// Create a `.code_index/` directory in the current working dir,
    /// scoping subsequent commands to a per-project DB. Subsequent
    /// `code-index ingest .` will write to `.code_index/index.db`
    /// instead of the global `~/.cache/code_index/<basename>.db`.
    #[command(after_long_help = INIT_AFTER_LONG_HELP)]
    Init,
}

#[derive(Subcommand, Debug)]
enum GraphOp {
    /// Populate edges from chunks via the chunker reference pass.
    #[command(after_long_help = "EXAMPLE:\n  code-index graph build   # re-parses every file; run after ingest\n\nDetail: code-index graph --help-ai")]
    Build,
    /// Quick overview: nodes, edges, components, degree.
    #[command(after_long_help = "EXAMPLE:\n  code-index graph stats\n\nDetail: code-index graph --help-ai")]
    Stats,
    /// List weakly-connected components, biggest first.
    #[command(after_long_help = "EXAMPLE:\n  code-index graph communities -n 10 --min-size 10\n\nDetail: code-index graph --help-ai")]
    Communities {
        /// Limit how many components to print.
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// Skip components below this node count.
        #[arg(long, default_value_t = 2)]
        min_size: usize,
    },
    /// Print shortest path between two chunk identifiers.
    #[command(after_long_help = "EXAMPLE:\n  code-index graph path 1421 8842   # chunk IDs from `recall` or sqlite\n\nDetail: code-index graph --help-ai")]
    Path { from: i64, to: i64 },
    /// PageRank-style centrality. Prints top-N chunks by score.
    #[command(after_long_help = "EXAMPLE:\n  code-index graph centrality -n 20\n\nDetail: code-index graph --help-ai")]
    Centrality {
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// PageRank damping factor. 0.85 is the canonical value.
        #[arg(long, default_value_t = 0.85)]
        damping: f32,
        /// Iteration count for the power-method update.
        #[arg(long, default_value_t = 50)]
        iterations: usize,
    },
}

// ---------------------------------------------------------------------------
// Help text. Examples are single-sourced here so the human `--help`
// (`after_long_help`) and the agent-facing `--help-ai` text share one copy;
// the `--json` form re-lists examples as an array (a deliberately separate
// representation). When you change an example, change it here.
// ---------------------------------------------------------------------------

const TOP_AFTER_LONG_HELP: &str = "\
TYPICAL ARC:
  cd <repo>
  code-index init                 # opt into a per-project .code_index/ DB (optional)
  code-index ingest .             # walk + chunk + embed
  code-index graph build          # extract call/reference edges
  code-index recall \"<behavior>\" -n 10 --full

EMBEDDINGS:
  Real embeddings need OPENROUTER_API_KEY (or CODE_INDEX_EMBED_MODEL /
  CODE_INDEX_EMBED_BASE_URL, falling back to AGENT_EMBED_MODEL / AGENT_EMBED_BASE_URL).
    export OPENROUTER_API_KEY=\"$(tq -f ~/.config/agent/config.toml -r openrouter.api_key)\"
  Without a key a MockEmbedder is used and recall is semantically meaningless.

AGENT-ORIENTED HELP:
  code-index --help-ai          # terse structured overview
  code-index --help-ai --json   # JSON overview
  code-index recall --help-ai   # per-command detail (output schema, sentinels)";

const RECALL_AFTER_LONG_HELP: &str = "\
EXAMPLES:
  code-index recall \"promote borrowed to owned\" -n 5 --full   # hybrid (default)
  code-index recall \"FixedBuffer\" --mode lexical               # exact symbol, no embeddings needed
  code-index recall \"where do we read parquet from S3\" -n 10   # concept / behavior

AGENT-ORIENTED HELP:
  code-index recall --help-ai          # terse structured text (output schema, sentinels)
  code-index recall --help-ai --json   # JSON";

const INGEST_AFTER_LONG_HELP: &str = "\
EXAMPLES:
  code-index ingest .                          # walk + chunk + embed cwd
  code-index ingest . --no-embed               # chunks only (offline / lexical-ready)
  code-index ingest . --embed-concurrency 8    # default; raise carefully (rate limits)
  code-index ingest vendored/ --no-gitignore   # index normally-ignored paths

Re-ingest is cheap: only files whose content hash changed are re-chunked, and
only chunks lacking an embedding for the target model are embedded.

AGENT-ORIENTED HELP:
  code-index ingest --help-ai [--json]";

const GRAPH_AFTER_LONG_HELP: &str = "\
TYPICAL ARC (after `code-index ingest .`):
  code-index graph build                          # extract edges (re-parses every file)
  code-index graph stats                          # nodes / edges / components / degree
  code-index graph centrality -n 20               # PageRank top-N
  code-index graph communities -n 10 --min-size 10
  code-index graph path <from-id> <to-id>         # shortest path between two chunk IDs

Find chunk IDs via `code-index recall ... --full` or sqlite on the chunks table.

AGENT-ORIENTED HELP:
  code-index graph --help-ai [--json]";

const STATUS_AFTER_LONG_HELP: &str = "\
EXAMPLE:
  code-index status            # DB path, file/chunk counts, embedding coverage, edges

AGENT-ORIENTED HELP:
  code-index status --help-ai [--json]";

const INIT_AFTER_LONG_HELP: &str = "\
EXAMPLE:
  cd <repo> && code-index init   # creates .code_index/; later commands auto-find it

Without init, code-index falls back to ~/.cache/code_index/<basename-of-cwd>.db.

AGENT-ORIENTED HELP:
  code-index init --help-ai [--json]";

const OVERVIEW_AI_TEXT: &str = "\
# code-index — code-aware indexing + retrieval (agent-oriented help)

PURPOSE
  Index a repo (chunk at definition boundaries, embed, store in sqlite) and
  retrieve relevant chunks by prose or symbol. Built to give an agent a
  focused slice of an unfamiliar codebase without re-reading every file.

WHEN TO USE WHICH
  recall (semantic/hybrid) : \"where do we X\", behavior described in prose.
  recall (lexical)         : exact symbol/keyword; works without embeddings.
  grep                     : when you know the exact token and want every hit.
  Ingest a repo before semantic/hybrid recall; lexical works on chunks alone.

PRECONDITIONS
  Real embeddings require OPENROUTER_API_KEY (or CODE_INDEX_EMBED_MODEL /
  CODE_INDEX_EMBED_BASE_URL -> AGENT_EMBED_MODEL / AGENT_EMBED_BASE_URL).
  Without a key a MockEmbedder is used: recall returns results but they are
  semantically meaningless.
    export OPENROUTER_API_KEY=\"$(tq -f ~/.config/agent/config.toml -r openrouter.api_key)\"

DB RESOLUTION (first match wins)
  1. --db <path>
  2. $CODE_INDEX_DB
  3. nearest .code_index/index.db walking up from cwd
  4. ~/.cache/code_index/<basename-of-cwd>.db

COMMANDS
  ingest <path>    walk + chunk + embed into the DB
  recall <query>   ranked retrieval; see: code-index recall --help-ai
  graph <op>       build|stats|communities|path|centrality over the ref graph
  status           files, chunks, embedding coverage by model
  init             create .code_index/ to scope a per-project DB

TYPICAL ARC
  cd <repo> && code-index init && code-index ingest . && code-index graph build
  code-index recall \"<behavior>\" -n 10 --full

PER-COMMAND DETAIL
  code-index recall --help-ai [--json]";

const RECALL_AI_TEXT: &str = "\
# code-index recall — ranked code retrieval (agent-oriented help)

PURPOSE
  Return indexed chunks ranked by relevance to QUERY. Hybrid fuses semantic
  (embedding cosine) and lexical (FTS5 BM25) via Reciprocal Rank Fusion.

USAGE
  code-index recall <QUERY> [-n N] [--full] [--mode hybrid|semantic|lexical]
                    [--no-test-penalty] [--exclude-tests] [--db PATH]

ARGS
  <QUERY>             prose description OR exact symbol/string (required)
  -n, --limit N       max results (default 10)
  --full              materialize and print chunk source text
  --mode M            hybrid (default) | semantic | lexical
  --no-test-penalty   stop the default 0.5x down-weight on test chunks
  --exclude-tests     drop test chunks from results entirely
  --db PATH           override DB (else $CODE_INDEX_DB, .code_index/, cache)

MODE SELECTION
  hybrid    default; safe for most queries
  semantic  prose / behavior (\"where do we read parquet from S3\")
  lexical   exact symbol/keyword (\"FixedBuffer\"); WORKS WITHOUT EMBEDDINGS,
            so it is usable mid-ingest before the embed pass finishes

OUTPUT (stdout; one line per hit)
  without --full:  \"<score:.4>  ChunkId(<id>)\"
  with --full:     \"<score:.4>  <Kind> <name>  <file>:<startLine>-<endLine>\"
                   then on following lines:  ---\\n<chunk source text>\\n---
  IMPORTANT: file/name/line metadata appears ONLY with --full. A bare recall
  prints score + ChunkId only.

SENTINELS / ERRORS
  \"(no hits — has the path been ingested with embeddings?)\"
      -> no embeddings for this model yet, or only-lexical chunks with
         mode != lexical. Run `code-index ingest .` (with a key) first,
         or retry with --mode lexical.
  invalid --mode value -> message on stderr, non-zero exit.

EXAMPLES
  code-index recall \"promote borrowed to owned\" -n 5 --full
  code-index recall \"FixedBuffer\" --mode lexical
  code-index recall \"where do we read parquet from S3\" -n 10";

const INGEST_AI_TEXT: &str = "\
# code-index ingest — walk, chunk, embed, persist (agent-oriented help)

PURPOSE
  Walk PATH, chunk each supported source file at definition boundaries via
  tree-sitter, embed the chunks, and upsert into the sqlite store.

USAGE
  code-index ingest <PATH> [--no-embed] [--embed-batch-size N]
                    [--embed-concurrency N] [--no-gitignore] [--db PATH]

ARGS
  <PATH>                  dir or file to walk (use \".\" for cwd) (required)
  --no-embed              chunk + persist only; skip the embedding pass
  --embed-batch-size N    chunks per embed request (default 16)
  --embed-concurrency N   in-flight embed HTTP requests (default 8)
  --no-gitignore          index paths normally excluded by .gitignore/.ignore
  --db PATH               override DB resolution

PRECONDITIONS
  Embedding needs OPENROUTER_API_KEY (or CODE_INDEX_EMBED_* / AGENT_EMBED_*).
  Without a key the MockEmbedder is used (results semantically meaningless).
  --no-embed avoids the API entirely; lexical recall still works afterward.

INCREMENTALITY
  Only files whose content hash changed are re-chunked. Only chunks lacking
  an embedding for the target model are embedded. Safe and cheap to re-run.

OUTPUT (stdout, one summary line)
  \"<path>: <N> files walked, <N> unchanged, <N> re-chunked, <N> chunks, <N> embedded\"

SUPPORTED LANGUAGES
  Rust (.rs), Python (.py, .pyi). Other files are skipped.

EXAMPLES
  code-index ingest .
  code-index ingest . --no-embed
  code-index ingest vendored/ --no-gitignore";

const GRAPH_AI_TEXT: &str = "\
# code-index graph — call/reference graph ops (agent-oriented help)

PURPOSE
  Build and analyze a directed graph of call/reference edges across chunks.
  Edges come from a re-parse pass that resolves tree-sitter reference tags by
  name lookup against the chunks table.

OPS
  build                                 extract edges (re-parses every manifest file)
  stats                                 nodes, edges, components, max/avg degree
  communities [-n N] [--min-size M]     weakly-connected components, biggest first
  path <FROM> <TO>                      shortest path between two chunk IDs
  centrality [-n N] [--damping D] [--iterations I]   PageRank top-N

PRECONDITION
  Run `code-index graph build` after ingest before stats/communities/path/
  centrality — they read the edges the build pass populates.

EDGE CONFIDENCE (v1 name resolution)
  same-file single match 1.0; cross-file single 0.85; ambiguous 0.85/0.6;
  no match -> unresolved (external / std / out-of-tree).

OUTPUT (stdout)
  build       : \"graph build: <N> files processed (<N> skipped), <N> references found, <N> edges emitted, <N> unresolved\"
  stats       : \"graph: <N> nodes, <N> edges, <N> components, max degree <N>, avg degree <f>\"
  communities : per component \"# Component <i> — <N> nodes\" then up to 10 \"<Kind> <name>  <file>:<line>\"
  path        : one line per hop \"ChunkId(<id>) <Kind> <name> <file>:<start>-<end>\", or \"no path ...\"
  centrality  : one line per chunk \"<score:.5>  <Kind> <name>  <file>:<start>-<end>\"

FINDING CHUNK IDS (for `path`)
  code-index recall \"<symbol>\" --full
  sqlite3 <db> \"SELECT id,kind,name,file FROM chunks WHERE name='X' LIMIT 5;\"

EXAMPLES
  code-index graph build
  code-index graph centrality -n 20
  code-index graph communities -n 10 --min-size 10
  code-index graph path 1421 8842";

const STATUS_AI_TEXT: &str = "\
# code-index status — index health (agent-oriented help)

PURPOSE
  Report what's indexed in the resolved DB: path, size, counts, embedding
  coverage by model, edge distribution. Read-only.

USAGE
  code-index status [--db PATH]

OUTPUT (stdout, key/value lines)
  db: <path>
  size: <bytes> bytes
  files: <N>
  chunks: <N>
  edges: <N>
  last indexed: <N> sec ago        (only if any chunk has a timestamp)
  chunk kinds:                     (then \"  <kind>: <N>\" lines)
  embeddings: none | per model \"  <model>: <N> (<pct>%)\"
  edge kinds:                      (only if edges > 0)

INTERPRETING
  embeddings \"none\" or low % -> semantic/hybrid recall will be empty/weak;
  run `code-index ingest .` with a key. edges 0 -> run `code-index graph build`.

EXAMPLE
  code-index status";

const INIT_AI_TEXT: &str = "\
# code-index init — scope a per-project DB (agent-oriented help)

PURPOSE
  Create a `.code_index/` directory in cwd so subsequent commands in this
  tree resolve to `.code_index/index.db` (walk-up discovery, git/jj-style)
  instead of the global `~/.cache/code_index/<basename>.db`.

USAGE
  code-index init

EFFECT
  Creates .code_index/index.db. Optional — without init, the global cache
  path is used. After init, ingest/recall/graph auto-find the project DB by
  walking up from cwd.

OUTPUT (stdout)
  \"initialized <dir> — subsequent code-index commands in this tree will use <dir>/index.db\"

EXAMPLE
  cd <repo> && code-index init && code-index ingest .";

/// Render agent-oriented help for the resolved (sub)command and return.
/// `None` (no subcommand) renders the top-level overview; each subcommand
/// renders its own doc. Graph subops all route to the consolidated graph
/// doc, which documents every op.
fn print_ai_help(command: Option<&Command>, json: bool) {
    let (text, json_fn): (&str, fn() -> String) = match command {
        Some(Command::Ingest { .. }) => (INGEST_AI_TEXT, ingest_ai_json),
        Some(Command::Recall { .. }) => (RECALL_AI_TEXT, recall_ai_json),
        Some(Command::Graph { .. }) => (GRAPH_AI_TEXT, graph_ai_json),
        Some(Command::Status) => (STATUS_AI_TEXT, status_ai_json),
        Some(Command::Init) => (INIT_AI_TEXT, init_ai_json),
        None => (OVERVIEW_AI_TEXT, overview_ai_json),
    };
    if json {
        println!("{}", json_fn());
    } else {
        println!("{text}");
    }
}

fn overview_ai_json() -> String {
    let v = serde_json::json!({
        "tool": "code-index",
        "purpose": "Index a repo (chunk at definition boundaries, embed, store in \
                    sqlite) and retrieve relevant chunks by prose or symbol.",
        "when_to_use": {
            "recall_semantic_or_hybrid": "behavior described in prose (\"where do we X\")",
            "recall_lexical": "exact symbol/keyword; works without embeddings",
            "grep": "when you know the exact token and want every hit"
        },
        "preconditions": {
            "embeddings_env": ["OPENROUTER_API_KEY", "CODE_INDEX_EMBED_MODEL",
                "CODE_INDEX_EMBED_BASE_URL", "AGENT_EMBED_MODEL", "AGENT_EMBED_BASE_URL"],
            "no_key_behavior": "MockEmbedder is used; recall results are semantically meaningless"
        },
        "db_resolution_order": [
            "--db <path>", "$CODE_INDEX_DB",
            "nearest .code_index/index.db walking up from cwd",
            "~/.cache/code_index/<basename-of-cwd>.db"
        ],
        "commands": {
            "ingest <path>": "walk + chunk + embed into the DB",
            "recall <query>": "ranked retrieval; see `code-index recall --help-ai --json`",
            "graph <op>": "build|stats|communities|path|centrality over the ref graph",
            "status": "files, chunks, embedding coverage by model",
            "init": "create .code_index/ to scope a per-project DB"
        },
        "typical_arc": [
            "cd <repo>", "code-index init", "code-index ingest .",
            "code-index graph build", "code-index recall \"<behavior>\" -n 10 --full"
        ]
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
}

fn recall_ai_json() -> String {
    let v = serde_json::json!({
        "command": "recall",
        "purpose": "Return indexed chunks ranked by relevance to QUERY. Hybrid fuses \
                    semantic (embedding cosine) and lexical (FTS5 BM25) via RRF.",
        "usage": "code-index recall <QUERY> [-n N] [--full] [--mode hybrid|semantic|lexical] \
                  [--no-test-penalty] [--exclude-tests] [--db PATH]",
        "args": [
            {"name": "query", "positional": true, "required": true,
             "desc": "prose description OR exact symbol/string"},
            {"name": "--limit/-n", "type": "usize", "default": 10, "desc": "max results"},
            {"name": "--full", "type": "flag", "desc": "materialize and print chunk source text"},
            {"name": "--mode", "type": "enum", "values": ["hybrid", "semantic", "lexical"],
             "default": "hybrid"},
            {"name": "--no-test-penalty", "type": "flag",
             "desc": "stop the default 0.5x down-weight on test chunks"},
            {"name": "--exclude-tests", "type": "flag", "desc": "drop test chunks entirely"},
            {"name": "--db", "type": "path", "desc": "override DB resolution"}
        ],
        "mode_selection": {
            "hybrid": "default; safe for most queries",
            "semantic": "prose/behavior",
            "lexical": "exact symbol/keyword; works WITHOUT embeddings (usable mid-ingest)"
        },
        "output_schema": {
            "stream": "stdout, one line per hit",
            "without_full": "<score:.4>  ChunkId(<id>)",
            "with_full": "<score:.4>  <Kind> <name>  <file>:<startLine>-<endLine>\n---\n<text>\n---",
            "note": "file/name/line metadata appears ONLY with --full"
        },
        "sentinels": [
            {"text": "(no hits — has the path been ingested with embeddings?)",
             "meaning": "no embeddings for this model, or only-lexical chunks with mode!=lexical",
             "remedy": "run `code-index ingest .` with a key, or retry --mode lexical"},
            {"text": "invalid --mode ...", "stream": "stderr", "exit": "non-zero"}
        ],
        "examples": [
            "code-index recall \"promote borrowed to owned\" -n 5 --full",
            "code-index recall \"FixedBuffer\" --mode lexical",
            "code-index recall \"where do we read parquet from S3\" -n 10"
        ]
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
}

fn ingest_ai_json() -> String {
    let v = serde_json::json!({
        "command": "ingest",
        "purpose": "Walk PATH, chunk source files at definition boundaries via \
                    tree-sitter, embed, and upsert into the sqlite store.",
        "usage": "code-index ingest <PATH> [--no-embed] [--embed-batch-size N] \
                  [--embed-concurrency N] [--no-gitignore] [--db PATH]",
        "args": [
            {"name": "path", "positional": true, "required": true,
             "desc": "dir or file to walk; use \".\" for cwd"},
            {"name": "--no-embed", "type": "flag", "desc": "chunk + persist only; skip embedding"},
            {"name": "--embed-batch-size", "type": "usize", "default": 16},
            {"name": "--embed-concurrency", "type": "usize", "default": 8},
            {"name": "--no-gitignore", "type": "flag", "desc": "index normally-ignored paths"},
            {"name": "--db", "type": "path", "desc": "override DB resolution"}
        ],
        "preconditions": {
            "embeddings_env": ["OPENROUTER_API_KEY", "CODE_INDEX_EMBED_MODEL",
                "CODE_INDEX_EMBED_BASE_URL", "AGENT_EMBED_MODEL", "AGENT_EMBED_BASE_URL"],
            "no_key_behavior": "MockEmbedder used; embeddings semantically meaningless",
            "no_embed_flag": "--no-embed skips the API entirely; lexical recall still works"
        },
        "incrementality": "only content-hash-changed files re-chunked; only chunks \
                           lacking an embedding for the model are embedded; cheap to re-run",
        "output_schema": {
            "stream": "stdout, one summary line",
            "format": "<path>: <N> files walked, <N> unchanged, <N> re-chunked, <N> chunks, <N> embedded"
        },
        "supported_languages": ["rust (.rs)", "python (.py, .pyi)"],
        "examples": [
            "code-index ingest .",
            "code-index ingest . --no-embed",
            "code-index ingest vendored/ --no-gitignore"
        ]
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
}

fn graph_ai_json() -> String {
    let v = serde_json::json!({
        "command": "graph",
        "purpose": "Build and analyze a directed call/reference graph across chunks. \
                    Edges resolved by name lookup from tree-sitter reference tags.",
        "precondition": "run `code-index graph build` after ingest before \
                         stats/communities/path/centrality",
        "ops": {
            "build": "extract edges (re-parses every manifest file)",
            "stats": "nodes, edges, components, max/avg degree",
            "communities": "weakly-connected components, biggest first; [-n N] [--min-size M]",
            "path": "shortest path between two chunk IDs; <FROM> <TO>",
            "centrality": "PageRank top-N; [-n N] [--damping D] [--iterations I]"
        },
        "edge_confidence": {
            "same_file_single": 1.0, "cross_file_single": 0.85,
            "ambiguous": "0.85 same-file else 0.6", "no_match": "unresolved (external/std/out-of-tree)"
        },
        "output_schema": {
            "build": "graph build: <N> files processed (<N> skipped), <N> references found, <N> edges emitted, <N> unresolved",
            "stats": "graph: <N> nodes, <N> edges, <N> components, max degree <N>, avg degree <f>",
            "communities": "per component: '# Component <i> — <N> nodes' then up to 10 '<Kind> <name>  <file>:<line>'",
            "path": "one line per hop 'ChunkId(<id>) <Kind> <name> <file>:<start>-<end>', or 'no path ...'",
            "centrality": "one line per chunk '<score:.5>  <Kind> <name>  <file>:<start>-<end>'"
        },
        "finding_chunk_ids": [
            "code-index recall \"<symbol>\" --full",
            "sqlite3 <db> \"SELECT id,kind,name,file FROM chunks WHERE name='X' LIMIT 5;\""
        ],
        "examples": [
            "code-index graph build",
            "code-index graph centrality -n 20",
            "code-index graph communities -n 10 --min-size 10",
            "code-index graph path 1421 8842"
        ]
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
}

fn status_ai_json() -> String {
    let v = serde_json::json!({
        "command": "status",
        "purpose": "Report what's indexed in the resolved DB. Read-only.",
        "usage": "code-index status [--db PATH]",
        "output_schema": {
            "stream": "stdout, key/value lines",
            "lines": [
                "db: <path>", "size: <bytes> bytes", "files: <N>", "chunks: <N>",
                "edges: <N>", "last indexed: <N> sec ago (if any chunk timestamped)",
                "chunk kinds: then '  <kind>: <N>' lines",
                "embeddings: 'none' or per model '  <model>: <N> (<pct>%)'",
                "edge kinds: (only if edges > 0)"
            ]
        },
        "interpreting": {
            "embeddings_none_or_low": "semantic/hybrid recall weak/empty; run ingest with a key",
            "edges_zero": "run `code-index graph build`"
        },
        "examples": ["code-index status"]
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
}

fn init_ai_json() -> String {
    let v = serde_json::json!({
        "command": "init",
        "purpose": "Create .code_index/ in cwd so subsequent commands resolve to a \
                    per-project DB (walk-up discovery) instead of the global cache.",
        "usage": "code-index init",
        "effect": "creates .code_index/index.db; optional; ingest/recall/graph then \
                   auto-find the project DB by walking up from cwd",
        "output_schema": {
            "stream": "stdout",
            "format": "initialized <dir> — subsequent code-index commands in this tree will use <dir>/index.db"
        },
        "examples": ["cd <repo> && code-index init && code-index ingest ."]
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
}

/// Resolve which DB path to use, in this precedence:
///   1. Explicit `--db` flag → use it verbatim.
///   2. `$CODE_INDEX_DB` env var → use it verbatim.
///   3. Walk up from cwd looking for a `.code_index/index.db` —
///      that's the per-project convention; first match wins.
///   4. Fall back to `~/.cache/code_index/<basename-of-cwd>.db`.
///
/// The discovery semantics match what git/jj/cargo do for their state
/// directories — declare scope explicitly with an `init`-style marker
/// directory, walk up to find it, and have a sensible per-tree fallback.
fn resolve_db_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Ok(env) = std::env::var("CODE_INDEX_DB") {
        if !env.is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    if let Some(p) = walk_up_for_marker() {
        return Ok(p);
    }
    Ok(global_default_for_cwd())
}

/// Walk up from the cwd looking for a `.code_index/index.db`.
fn walk_up_for_marker() -> Option<PathBuf> {
    let mut cur = std::env::current_dir().ok()?;
    loop {
        let candidate = cur.join(".code_index").join("index.db");
        if candidate.exists() {
            return Some(candidate);
        }
        // Also accept the dir-without-db case: the user ran `code-index init`
        // but hasn't ingested yet. We point at the path so SqliteStore
        // creates it.
        let dir = cur.join(".code_index");
        if dir.is_dir() {
            return Some(candidate);
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// `~/.cache/code_index/<basename-of-cwd>.db` fallback. If cwd basename
/// can't be derived, uses `index.db`.
fn global_default_for_cwd() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let cwd = std::env::current_dir().ok();
    let stem = cwd
        .as_ref()
        .and_then(|c| c.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("index");
    PathBuf::from(home)
        .join(".cache")
        .join("code_index")
        .join(format!("{stem}.db"))
}

fn open_store(explicit_db: Option<&Path>) -> Result<SqliteStore> {
    let path = resolve_db_path(explicit_db)?;
    SqliteStore::open_at(&path)
}

fn print_component(
    store: &dyn Store,
    index: usize,
    comp: &[ChunkId],
) -> Result<()> {
    println!("# Component {index} — {} nodes", comp.len());
    // Sample up to 10 names so big components don't spam output.
    let sample = comp.iter().take(10);
    for id in sample {
        if let Some(c) = store.get_chunk(*id)? {
            println!(
                "  {:?} {}  {}:{}",
                c.kind,
                c.name,
                c.file.display(),
                c.lines.start,
            );
        }
    }
    if comp.len() > 10 {
        println!("  ... and {} more", comp.len() - 10);
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // `--help-ai` short-circuits before any dispatch: it documents the
    // (sub)command and exits, exactly like clap's own `--help`.
    if cli.help_ai {
        print_ai_help(cli.command.as_ref(), cli.json);
        return Ok(());
    }

    // No subcommand and not `--help-ai`: reproduce clap's default
    // print-help-and-exit behavior (the subcommand is Optional only so
    // `--help-ai` can stand alone).
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    match command {
        Command::Ingest {
            path,
            no_embed,
            embed_batch_size,
            embed_concurrency,
            no_gitignore,
        } => {
            // `path` is Optional only so `ingest --help-ai` can parse.
            let path = path.ok_or_else(|| {
                anyhow::anyhow!("ingest requires a <PATH> (use '.' for cwd, or --help-ai for usage)")
            })?;
            let mut store = open_store(cli.db.as_deref())?;
            let stats = ingest_with(&path, &mut store, None, !no_gitignore)?;
            let embedded = if no_embed {
                0
            } else {
                let embedder = select_embedder();
                embed_pending_concurrent(
                    &mut store,
                    embedder.as_ref(),
                    embed_batch_size,
                    embed_concurrency,
                )?
            };
            println!(
                "{}: {} files walked, {} unchanged, {} re-chunked, {} chunks, {} embedded",
                path.display(),
                stats.files_walked,
                stats.files_unchanged,
                stats.files_chunked,
                stats.chunks_upserted,
                embedded,
            );
            Ok(())
        }
        Command::Recall {
            query,
            limit,
            full,
            mode,
            no_test_penalty,
            exclude_tests,
        } => {
            // `query` is Optional only so `recall --help-ai` can parse
            // without one; an actual recall requires it.
            let query = query.ok_or_else(|| {
                anyhow::anyhow!("recall requires a <QUERY> (or pass --help-ai for usage)")
            })?;
            let store = open_store(cli.db.as_deref())?;
            let embedder = select_embedder();
            let mode = RecallMode::from_str(&mode)
                .ok_or_else(|| anyhow::anyhow!("invalid --mode {mode:?}; expected hybrid|semantic|lexical"))?;
            let tuning = RecallTuning {
                test_penalty: if no_test_penalty { 1.0 } else { DEFAULT_TEST_PENALTY },
                exclude_tests,
            };
            let hits =
                recall_tuned(&store, embedder.as_ref(), &query, limit, full, mode, tuning)?;
            if hits.is_empty() {
                println!("(no hits — has the path been ingested with embeddings?)");
            }
            for h in hits {
                if let Some(c) = &h.chunk {
                    println!(
                        "{:.4}  {:?} {}  {}:{}-{}",
                        h.score,
                        c.kind,
                        c.name,
                        c.file.display(),
                        c.lines.start,
                        c.lines.end,
                    );
                    if full {
                        println!("---");
                        println!("{}", c.text);
                        println!("---");
                    }
                } else {
                    println!("{:.4}  ChunkId({})", h.score, h.id.0);
                }
            }
            Ok(())
        }
        Command::Graph { op } => {
            // `op` is Optional only so `graph --help-ai` can parse.
            let op = op.ok_or_else(|| {
                anyhow::anyhow!(
                    "graph requires a subcommand: build|stats|communities|path|centrality \
                     (or pass --help-ai for usage)"
                )
            })?;
            match op {
            GraphOp::Build => {
                let mut store = open_store(cli.db.as_deref())?;
                let stats = build_edges(&mut store)?;
                println!(
                    "graph build: {} files processed ({} skipped), {} references found, {} edges emitted, {} unresolved",
                    stats.files_processed,
                    stats.files_skipped,
                    stats.references_found,
                    stats.edges_emitted,
                    stats.references_unresolved,
                );
                Ok(())
            }
            GraphOp::Stats => {
                let store = open_store(cli.db.as_deref())?;
                let g = Graph::from_store(&store)?;
                let s = g.stats();
                println!(
                    "graph: {} nodes, {} edges, {} components, max degree {}, avg degree {:.2}",
                    s.nodes, s.edges, s.components, s.max_degree, s.avg_degree,
                );
                Ok(())
            }
            GraphOp::Communities { limit, min_size } => {
                let store = open_store(cli.db.as_deref())?;
                let g = Graph::from_store(&store)?;
                let comps = g.connected_components();
                let total = comps.len();
                let mut printed = 0;
                for (i, comp) in comps.iter().enumerate() {
                    if comp.len() < min_size {
                        continue;
                    }
                    if printed >= limit {
                        break;
                    }
                    print_component(&store, i, comp)?;
                    printed += 1;
                }
                println!(
                    "(printed {printed} of {total} components; showing components with >= {min_size} nodes)",
                );
                Ok(())
            }
            GraphOp::Path { from, to } => {
                let store = open_store(cli.db.as_deref())?;
                let g = Graph::from_store(&store)?;
                match g.shortest_path(ChunkId(from), ChunkId(to)) {
                    None => {
                        println!("no path from ChunkId({from}) to ChunkId({to})");
                    }
                    Some(path) => {
                        for id in path {
                            if let Some(c) = store.get_chunk(id)? {
                                println!(
                                    "ChunkId({}) {:?} {} {}:{}-{}",
                                    id.0,
                                    c.kind,
                                    c.name,
                                    c.file.display(),
                                    c.lines.start,
                                    c.lines.end,
                                );
                            } else {
                                println!("ChunkId({}) [chunk missing]", id.0);
                            }
                        }
                    }
                }
                Ok(())
            }
            GraphOp::Centrality {
                limit,
                damping,
                iterations,
            } => {
                let store = open_store(cli.db.as_deref())?;
                let g = Graph::from_store(&store)?;
                let ranks = g.pagerank(damping, iterations);
                for (id, score) in ranks.into_iter().take(limit) {
                    if let Some(c) = store.get_chunk(id)? {
                        println!(
                            "{score:.5}  {:?} {}  {}:{}-{}",
                            c.kind,
                            c.name,
                            c.file.display(),
                            c.lines.start,
                            c.lines.end,
                        );
                    } else {
                        println!("{score:.5}  ChunkId({})", id.0);
                    }
                }
                Ok(())
            }
            }
        }
        Command::Status => {
            let path = resolve_db_path(cli.db.as_deref())?;
            let store = SqliteStore::open_at(&path)?;
            print_status(&path, &store)?;
            Ok(())
        }
        Command::Init => {
            let cwd = std::env::current_dir()?;
            let dir = cwd.join(".code_index");
            std::fs::create_dir_all(&dir)?;
            // Touch the DB so subsequent commands resolve to it cleanly.
            let _store = SqliteStore::open_at(&dir.join("index.db"))?;
            println!(
                "initialized {} — subsequent code-index commands in this tree will use {}/index.db",
                dir.display(),
                dir.display(),
            );
            Ok(())
        }
    }
}

fn print_status(path: &Path, store: &dyn Store) -> Result<()> {
    use rusqlite::Connection;
    println!("db: {}", path.display());

    if let Ok(meta) = std::fs::metadata(path) {
        println!("size: {} bytes", meta.len());
    }

    // Most stats are easier via direct SQL than threading a dozen new
    // trait methods through the Store interface. Status is read-only
    // and intentionally bypasses the Store trait for one-off queries.
    let conn = Connection::open(path)?;

    let file_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM file_manifest", [], |r| r.get(0))
        .unwrap_or(0);
    let chunk_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .unwrap_or(0);
    let edge_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
        .unwrap_or(0);
    let last_indexed: Option<i64> = conn
        .query_row("SELECT MAX(indexed_at) FROM chunks", [], |r| r.get(0))
        .ok();

    println!("files: {file_count}");
    println!("chunks: {chunk_count}");
    println!("edges: {edge_count}");
    if let Some(ts) = last_indexed {
        let age = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64 - ts)
            .unwrap_or(0);
        println!("last indexed: {} sec ago", age.max(0));
    }

    // Chunk distribution by kind.
    let mut stmt = conn
        .prepare("SELECT kind, COUNT(*) FROM chunks GROUP BY kind ORDER BY 2 DESC")?;
    let rows = stmt.query_map([], |r| {
        let k: String = r.get(0)?;
        let c: i64 = r.get(1)?;
        Ok((k, c))
    })?;
    let kinds: Vec<(String, i64)> = rows.filter_map(|r| r.ok()).collect();
    if !kinds.is_empty() {
        println!("chunk kinds:");
        for (k, c) in kinds {
            println!("  {k}: {c}");
        }
    }

    // Embedding coverage by model.
    let mut stmt = conn
        .prepare("SELECT model, COUNT(*) FROM chunk_embeddings GROUP BY model")?;
    let rows = stmt.query_map([], |r| {
        let m: String = r.get(0)?;
        let c: i64 = r.get(1)?;
        Ok((m, c))
    })?;
    let models: Vec<(String, i64)> = rows.filter_map(|r| r.ok()).collect();
    if models.is_empty() {
        println!("embeddings: none");
    } else {
        println!("embeddings:");
        for (m, c) in models {
            let pct = if chunk_count > 0 {
                (c as f64 / chunk_count as f64) * 100.0
            } else {
                0.0
            };
            println!("  {m}: {c} ({pct:.1}%)");
        }
    }

    // Edge distribution by kind.
    if edge_count > 0 {
        let mut stmt = conn.prepare(
            "SELECT kind, COUNT(*) FROM edges GROUP BY kind ORDER BY 2 DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            let k: String = r.get(0)?;
            let c: i64 = r.get(1)?;
            Ok((k, c))
        })?;
        let edges: Vec<(String, i64)> = rows.filter_map(|r| r.ok()).collect();
        println!("edge kinds:");
        for (k, c) in edges {
            println!("  {k}: {c}");
        }
    }

    let _ = store; // suppress unused — reserved for future trait-method use
    Ok(())
}
