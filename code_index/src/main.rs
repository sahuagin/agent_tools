//! `code-index` CLI entry point.
//!
//! Verb skeleton only — every body is `todo!()` for the scaffold commit.
//! Logic fills in module-by-module in subsequent commits.

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
    /// Path to the index database (default: $XDG_DATA_HOME/code_index/index.db).
    #[arg(long, global = true)]
    db: Option<std::path::PathBuf>,

    /// Analyzer backend to use for graph operations.
    #[arg(long, global = true, default_value = "petgraph")]
    analyzer: String,

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
    /// What's indexed, when, how big.
    Status,
    /// Available analyzer backends and the active default.
    Analyzer {
        #[command(subcommand)]
        op: AnalyzerOp,
    },
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

#[derive(Subcommand, Debug)]
enum AnalyzerOp {
    /// List analyzers available in this build.
    List,
    /// Set the default analyzer (persisted in the store's config).
    Set { name: String },
}

fn open_store(db: Option<&std::path::Path>) -> Result<SqliteStore> {
    match db {
        Some(p) => SqliteStore::open_at(p),
        None => SqliteStore::open_default(),
    }
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
        Command::Status => todo!("status: index size, last update, file count"),
        Command::Analyzer { op } => match op {
            AnalyzerOp::List => todo!("analyzer list"),
            AnalyzerOp::Set { name } => {
                let _ = name;
                todo!("analyzer set")
            }
        },
    }
}
