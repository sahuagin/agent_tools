//! code-index-mcp: MCP server exposing code-index recall.
//!
//! Replaces code-index-lsp with MCP protocol. Same underlying recall
//! engine, same multi-db resolution, different transport.
//!
//! Launch:  code-index-mcp              (stdio, Claude Code spawns as subprocess)
//! Env:     CODE_INDEX_DB=path/to/index.db

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};

use code_index::embed::select_embedder;
use code_index::recall::{self, RecallMode, RecallTuning, DEFAULT_TEST_PENALTY};
use code_index::store::SqliteStore;

const MIN_POPULATED_DB_SIZE: u64 = 100_000;

fn db_is_populated(path: &Path) -> bool {
    path.metadata()
        .map_or(false, |m| m.len() >= MIN_POPULATED_DB_SIZE)
}

fn resolve_db() -> PathBuf {
    if let Some(p) = std::env::var_os("CODE_INDEX_DB") {
        return PathBuf::from(p);
    }
    if let Some(p) = walk_up_for_marker() {
        return p;
    }
    global_default_for_cwd()
}

fn walk_up_for_marker() -> Option<PathBuf> {
    let mut cur = std::env::current_dir().ok()?;
    loop {
        let candidate = cur.join(".code_index").join("index.db");
        if db_is_populated(&candidate) {
            return Some(candidate);
        }
        if !cur.pop() {
            return None;
        }
    }
}

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

// ─── Tool parameter types ───────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RecallParams {
    /// Natural language query or symbol name to search for
    query: String,
    /// Max results to return (default 10)
    #[serde(default = "default_limit")]
    limit: usize,
    /// Recall strategy: hybrid (default), semantic, or lexical
    #[serde(default = "default_mode")]
    mode: String,
    /// Drop test files from results entirely
    #[serde(default)]
    exclude_tests: bool,
    /// Override the default DB path. Use a repo name (e.g. "mu") to
    /// resolve ~/.cache/code_index/<name>.db, or an absolute path.
    #[serde(default)]
    db: Option<String>,
}

fn default_limit() -> usize {
    10
}
fn default_mode() -> String {
    "hybrid".into()
}

// ─── Server ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct CodeIndexServer {
    db_path: Arc<PathBuf>,
}

#[tool_router]
impl CodeIndexServer {
    fn new(db_path: PathBuf) -> Self {
        Self {
            db_path: Arc::new(db_path),
        }
    }

    #[tool(
        description = "Preferred over grep for code search. Semantic + lexical hybrid retrieval over an indexed codebase. Returns ranked source code chunks with file paths and line numbers. Use this for finding symbols, types, functions, concepts, or patterns instead of grep/ripgrep."
    )]
    async fn code_recall(
        &self,
        Parameters(params): Parameters<RecallParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let db_path = match &params.db {
            Some(name) if name.starts_with('/') => Arc::new(PathBuf::from(name)),
            Some(name) => {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                Arc::new(
                    PathBuf::from(home)
                        .join(".cache/code_index")
                        .join(format!("{name}.db")),
                )
            }
            None => self.db_path.clone(),
        };
        let result = tokio::task::spawn_blocking(move || recall_blocking(&db_path, &params))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("task join: {e}"), None))?;

        match result {
            Ok(text) => Ok(CallToolResult::success(vec![Content::text(text)])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    #[tool(
        description = "Show code index status: DB path, file count, chunk count, embedding coverage."
    )]
    async fn code_status(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let db_path = self.db_path.clone();
        let result = tokio::task::spawn_blocking(move || status_blocking(&db_path))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("task join: {e}"), None))?;

        match result {
            Ok(text) => Ok(CallToolResult::success(vec![Content::text(text)])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }
}

#[tool_handler]
impl ServerHandler for CodeIndexServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("code-index", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Code index server. Use code_recall to search for symbols, concepts, \
                 or patterns in indexed repositories. Use code_status to check index health.",
            )
    }
}

// ─── Recall logic (reused from lsp.rs) ──────────────────────────────

fn recall_blocking(db_path: &Path, params: &RecallParams) -> Result<String, String> {
    let mode = RecallMode::from_str(&params.mode)
        .ok_or_else(|| format!("invalid mode: {}", params.mode))?;

    let tuning = RecallTuning {
        test_penalty: DEFAULT_TEST_PENALTY,
        exclude_tests: params.exclude_tests,
    };

    let store = SqliteStore::open_at(db_path).map_err(|e| format!("open db: {e}"))?;
    let embedder = select_embedder();

    let hits = recall::recall_tuned(
        &store,
        embedder.as_ref(),
        &params.query,
        params.limit,
        true,
        mode,
        tuning,
    )
    .or_else(|_| {
        recall::recall_tuned(
            &store,
            embedder.as_ref(),
            &params.query,
            params.limit,
            true,
            RecallMode::Lexical,
            tuning,
        )
    })
    .map_err(|e| format!("recall error: {e}"))?;

    if hits.is_empty() {
        return Ok(
            "No results found. Has the repository been indexed? Run: code-index ingest .".into(),
        );
    }

    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        if let Some(c) = &h.chunk {
            out.push_str(&format!(
                "## {} ({:.3}) {:?} — {}:{}-{}\n\n```\n{}\n```\n\n",
                c.name,
                h.score,
                c.kind,
                c.file.display(),
                c.lines.start,
                c.lines.end,
                c.text
            ));
            if i < hits.len() - 1 {
                out.push_str("---\n\n");
            }
        }
    }
    Ok(out)
}

fn status_blocking(db_path: &Path) -> Result<String, String> {
    let _store = SqliteStore::open_at(db_path).map_err(|e| format!("open db: {e}"))?;
    let conn =
        rusqlite::Connection::open(db_path).map_err(|e| format!("open db for status: {e}"))?;

    let file_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM file_manifest", [], |r| r.get(0))
        .unwrap_or(0);
    let chunk_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .unwrap_or(0);
    let edge_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
        .unwrap_or(0);

    let mut embed_info = String::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT model, COUNT(*) FROM chunk_embeddings GROUP BY model")
    {
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        for (model, count) in rows {
            let pct = if chunk_count > 0 {
                (count as f64 / chunk_count as f64) * 100.0
            } else {
                0.0
            };
            embed_info.push_str(&format!("  {model}: {count} ({pct:.1}%)\n"));
        }
    }

    Ok(format!(
        "db: {}\nfiles: {}\nchunks: {}\nedges: {}\nembeddings:\n{}",
        db_path.display(),
        file_count,
        chunk_count,
        edge_count,
        if embed_info.is_empty() {
            "  none\n".into()
        } else {
            embed_info
        }
    ))
}

// ─── Entry point ────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db_path = resolve_db();

    let listen = std::env::args().nth(1);
    match listen.as_deref() {
        Some(addr) if addr.starts_with("--listen=") => {
            let addr = &addr["--listen=".len()..];
            serve_http(addr, db_path).await
        }
        Some("--listen") => {
            let addr = std::env::args().nth(2).unwrap_or_else(|| {
                eprintln!("usage: code-index-mcp --listen <host:port>");
                std::process::exit(1);
            });
            serve_http(&addr, db_path).await
        }
        _ => {
            eprintln!("code-index-mcp: db={} (stdio)", db_path.display());
            let server = CodeIndexServer::new(db_path)
                .serve(rmcp::transport::stdio())
                .await?;
            server.waiting().await?;
            Ok(())
        }
    }
}

async fn serve_http(addr: &str, db_path: PathBuf) -> anyhow::Result<()> {
    use axum::Router;
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, tower::StreamableHttpService,
    };

    use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;

    let db = db_path.clone();
    let mut config = StreamableHttpServerConfig::default();
    config.allowed_hosts = vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "::1".into(),
        "10.1.1.172".into(),
        format!("10.1.1.172:{}", addr.rsplit(':').next().unwrap_or("7622")),
        addr.to_string(),
    ];
    let service: StreamableHttpService<CodeIndexServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(CodeIndexServer::new(db.clone())),
            LocalSessionManager::default().into(),
            config,
        );

    let app = Router::new().nest_service("/mcp", service);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!(
        "code-index-mcp: db={} listening on http://{}",
        db_path.display(),
        addr
    );

    axum::serve(listener, app).await?;
    Ok(())
}
