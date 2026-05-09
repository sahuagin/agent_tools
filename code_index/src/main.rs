//! `code-index` CLI entry point.
//!
//! Verb skeleton only — every body is `todo!()` for the scaffold commit.
//! Logic fills in module-by-module in subsequent commits.

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    },
    /// Semantic recall over indexed chunks. Returns ranked (id, score) pairs.
    Recall {
        query: String,
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        /// Materialize and print chunk contents for results.
        #[arg(long)]
        full: bool,
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
    /// Populate edges from chunks using the current analyzer.
    Build,
    /// List detected communities.
    Communities,
    /// Print shortest path between two chunk identifiers.
    Path { from: i64, to: i64 },
    /// Centrality scores per chunk.
    Centrality,
}

#[derive(Subcommand, Debug)]
enum AnalyzerOp {
    /// List analyzers available in this build.
    List,
    /// Set the default analyzer (persisted in the store's config).
    Set { name: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Ingest { path } => {
            let _ = path;
            todo!("ingest: walk + chunk + embed + persist")
        }
        Command::Recall { query, limit, full } => {
            let _ = (query, limit, full);
            todo!("recall: embed query + Store::recall_top_k + optional materialize")
        }
        Command::Graph { op } => match op {
            GraphOp::Build => todo!("graph build: hydrate Graph + analyzer.build_edges"),
            GraphOp::Communities => todo!("graph communities: analyzer.community_detection"),
            GraphOp::Path { from, to } => {
                let _ = (from, to);
                todo!("graph path: analyzer.shortest_path")
            }
            GraphOp::Centrality => todo!("graph centrality: analyzer.centrality"),
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
