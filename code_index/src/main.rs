//! `code-index` CLI entry point.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use code_index::edges::build_edges;
use code_index::embed::{embed_pending_concurrent, select_embedder};
use code_index::graph::Graph;
use code_index::ingest::ingest_with;
use code_index::recall::{recall_with_mode, RecallMode};
use code_index::store::SqliteStore;
use code_index::{ChunkId, Store};

#[derive(Parser, Debug)]
#[command(
    name = "code-index",
    version,
    about = "Code-aware indexing and retrieval for agentic workflows."
)]
struct Cli {
    /// Path to the index database. If omitted, code-index walks up from
    /// the current directory looking for a `.code_index/index.db`; if not
    /// found, falls back to `~/.cache/code_index/<basename-of-cwd>.db`.
    /// Set `CODE_INDEX_DB` env var to override the discovery without
    /// passing `--db` on every invocation.
    #[arg(long, global = true)]
    db: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Walk a path, chunk via tree-sitter, embed, and persist to the store.
    Ingest {
        path: std::path::PathBuf,
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
    Recall {
        query: String,
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// Materialize and print chunk contents for results.
        #[arg(long)]
        full: bool,
        /// Recall strategy: `hybrid` (default, semantic + lexical via RRF),
        /// `semantic` (embedding cosine only), or `lexical` (FTS5 BM25 only).
        #[arg(long, default_value = "hybrid")]
        mode: String,
    },
    /// Graph operations — build edges, run analyzers, inspect communities.
    Graph {
        #[command(subcommand)]
        op: GraphOp,
    },
    /// What's indexed, when, how big. Prints DB path, file count, chunk
    /// distribution by kind, embeddings by model, edge distribution.
    Status,
    /// Create a `.code_index/` directory in the current working dir,
    /// scoping subsequent commands to a per-project DB. Subsequent
    /// `code-index ingest .` will write to `.code_index/index.db`
    /// instead of the global `~/.cache/code_index/<basename>.db`.
    Init,
}

#[derive(Subcommand, Debug)]
enum GraphOp {
    /// Populate edges from chunks via the chunker reference pass.
    Build,
    /// Quick overview: nodes, edges, components, degree.
    Stats,
    /// List weakly-connected components, biggest first.
    Communities {
        /// Limit how many components to print.
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// Skip components below this node count.
        #[arg(long, default_value_t = 2)]
        min_size: usize,
    },
    /// Print shortest path between two chunk identifiers.
    Path { from: i64, to: i64 },
    /// PageRank-style centrality. Prints top-N chunks by score.
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
    match cli.command {
        Command::Ingest {
            path,
            no_embed,
            embed_batch_size,
            embed_concurrency,
            no_gitignore,
        } => {
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
        } => {
            let store = open_store(cli.db.as_deref())?;
            let embedder = select_embedder();
            let mode = RecallMode::from_str(&mode)
                .ok_or_else(|| anyhow::anyhow!("invalid --mode {mode:?}; expected hybrid|semantic|lexical"))?;
            let hits = recall_with_mode(&store, embedder.as_ref(), &query, limit, full, mode)?;
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
        Command::Graph { op } => match op {
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
        },
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
