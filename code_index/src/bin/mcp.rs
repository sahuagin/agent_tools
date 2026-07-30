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
use code_index::sources::{self, Sources, MIN_POPULATED_DB_SIZE};
use code_index::store::SqliteStore;

fn db_is_populated(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.len() >= MIN_POPULATED_DB_SIZE)
}

fn resolve_db() -> PathBuf {
    if let Some(p) = std::env::var("CODE_INDEX_DB")
        .ok()
        .filter(|s| !s.is_empty())
    {
        // Expanded, so a tilde in the service env never becomes a literal
        // `~` directory (see sources::Sources::resolve).
        return sources::expand_home(&p);
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
    let cwd = std::env::current_dir().ok();
    let stem = cwd
        .as_ref()
        .and_then(|c| c.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("index")
        .to_string();
    sources::default_cache_dir().join(format!("{stem}.db"))
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

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
struct StatusParams {
    /// Which index to report on: a source name (e.g. "mu") or an absolute
    /// path. Omit for the service's default index.
    #[serde(default)]
    db: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
struct SourcesParams {
    /// Machine-readable output: one TAB-separated
    /// `name<TAB>managed|unmanaged<TAB>path<TAB>detail` row per index, no
    /// header. For programmatic consumers (the mesh service); humans and
    /// models want the default rendering.
    #[serde(default)]
    porcelain: bool,
}

// ─── Server ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct CodeIndexServer {
    db_path: Arc<PathBuf>,
    sources: Arc<Sources>,
}

#[tool_router]
impl CodeIndexServer {
    fn new(db_path: PathBuf) -> Self {
        Self {
            db_path: Arc::new(db_path),
            sources: Arc::new(Sources::load()),
        }
    }

    /// Resolve a caller-supplied `db` argument, or fall back to the default.
    /// Errors are returned as text so the caller sees the resolved path and
    /// the available alternatives instead of an empty result set.
    fn resolve_arg(&self, db: Option<&String>) -> Result<Arc<PathBuf>, String> {
        match db {
            Some(name) => self
                .sources
                .resolve(name)
                .map(Arc::new)
                .map_err(|e| e.to_string()),
            None => Ok(self.db_path.clone()),
        }
    }

    #[tool(
        description = "Preferred over grep for code search. Semantic + lexical hybrid retrieval over an indexed codebase. Returns ranked source code chunks with file paths and line numbers. Use this for finding symbols, types, functions, concepts, or patterns instead of grep/ripgrep."
    )]
    async fn code_recall(
        &self,
        Parameters(params): Parameters<RecallParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let db_path = match self.resolve_arg(params.db.as_ref()) {
            Ok(p) => p,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(e)])),
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
        description = "Show code index status: DB path, file count, chunk count, embedding coverage. Pass `db` to report on a specific index; omit for the default. Use code_sources to see which indexes exist."
    )]
    async fn code_status(
        &self,
        Parameters(params): Parameters<StatusParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let db_path = match self.resolve_arg(params.db.as_ref()) {
            Ok(p) => p,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(e)])),
        };
        let result = tokio::task::spawn_blocking(move || status_blocking(&db_path))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(format!("task join: {e}"), None))?;

        match result {
            Ok(text) => Ok(CallToolResult::success(vec![Content::text(text)])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    #[tool(
        description = "List the code indexes this server can serve: the name to pass as `db`, the repository it indexes, and how fresh it is. Call this first instead of guessing a repo name."
    )]
    async fn code_sources(
        &self,
        Parameters(params): Parameters<SourcesParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let sources = self.sources.clone();
        let default = self.db_path.clone();
        let text = tokio::task::spawn_blocking(move || {
            if params.porcelain {
                sources_porcelain(&sources)
            } else {
                sources_blocking(&sources, &default)
            }
        })
        .await
        .map_err(|e| rmcp::ErrorData::internal_error(format!("task join: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_handler]
impl ServerHandler for CodeIndexServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("code-index", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Code index server. Use code_recall to search for symbols, concepts, \
                 or patterns in indexed repositories. Call code_sources to see which \
                 repositories are indexed and what to pass as `db` — do not guess a \
                 repo name. Use code_status to check one index's health.",
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

    let store = SqliteStore::open_existing_at(db_path).map_err(|e| format!("open db: {e}"))?;
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
        // The index opened, so it exists and is populated — this is a genuine
        // miss, not a missing database. (A missing one is a typed error now.)
        return Ok(format!(
            "No matches for {:?} in {}. Try different terms, mode=lexical for \
             exact symbols, or code_sources to pick a different index.",
            params.query,
            db_path.display()
        ));
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
    if !db_path.is_file() {
        return Err(format!(
            "no index at {} (not creating it)",
            db_path.display()
        ));
    }
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .or_else(|_| rusqlite::Connection::open(db_path))
    .map_err(|e| format!("open db for status: {e}"))?;

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

/// Render the servable index set: configured sources first, then any other
/// populated db already in the cache family (those still work, they are just
/// unmanaged — no cron keeps them fresh).
fn sources_blocking(sources: &Sources, default_db: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "cache dir: {}\ndefault db: {}{}\n\n",
        sources.cache_dir().display(),
        default_db.display(),
        if default_db.is_file() {
            ""
        } else {
            "   [MISSING]"
        },
    ));

    if sources.entries().is_empty() {
        out.push_str(
            "configured sources: none — add [[code_index.sources]] to \
             ~/.config/agent/config.toml so the reindex cron and this service \
             agree on what is indexed.\n\n",
        );
    } else {
        out.push_str("configured sources:\n");
        for s in sources.entries() {
            let db = s.db_path(sources.cache_dir());
            out.push_str(&format!(
                "  {:<20} {:<10} {:<52} {}\n",
                s.name,
                if s.repo { "repo" } else { "content" },
                s.path.display(),
                describe_db(&db),
            ));
        }
        out.push('\n');
    }

    let configured: Vec<&str> = sources.entries().iter().map(|s| s.name.as_str()).collect();
    let extra: Vec<String> = sources
        .discovered()
        .into_iter()
        .filter(|n| !configured.contains(&n.as_str()))
        .collect();
    if !extra.is_empty() {
        out.push_str("unmanaged indexes in the cache dir (servable, not maintained):\n");
        for name in extra {
            let db = sources.cache_dir().join(format!("{name}.db"));
            out.push_str(&format!("  {:<20} {}\n", name, describe_db(&db)));
        }
    }
    out
}

/// Machine-readable source listing, one row per index:
/// `name<TAB>managed|unmanaged<TAB>path<TAB>detail`.
///
/// `managed` = configured in `[[code_index.sources]]`, so the reindex cron
/// keeps it fresh. `unmanaged` = present in the cache dir and servable, but
/// nothing maintains it. For unmanaged rows `path` is the db itself, since
/// there is no configured root to name.
fn sources_porcelain(sources: &Sources) -> String {
    let mut out = String::new();
    for s in sources.entries() {
        let db = s.db_path(sources.cache_dir());
        out.push_str(&format!(
            "{}\tmanaged\t{}\t{}\n",
            s.name,
            s.path.display(),
            describe_db(&db),
        ));
    }
    let configured: Vec<&str> = sources.entries().iter().map(|s| s.name.as_str()).collect();
    for name in sources.discovered() {
        if configured.contains(&name.as_str()) {
            continue;
        }
        let db = sources.cache_dir().join(format!("{name}.db"));
        out.push_str(&format!(
            "{}\tunmanaged\t{}\t{}\n",
            name,
            db.display(),
            describe_db(&db),
        ));
    }
    out
}

/// One-line contents/freshness summary for a db file.
///
/// Counts rows rather than judging by file size: a small repo has a small
/// index, and calling that an "empty shell" would be a false alarm. An index
/// with zero chunks is the real defect — that is the open-created ghost.
fn describe_db(db: &Path) -> String {
    let Ok(meta) = db.metadata() else {
        return "MISSING".to_string();
    };
    let age = meta
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .map(|d| {
            let hours = d.as_secs() / 3600;
            if hours < 48 {
                format!("{hours}h ago")
            } else {
                format!("{}d ago", hours / 24)
            }
        })
        .unwrap_or_else(|| "?".into());

    let counts = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
    .and_then(|c| {
        let files: i64 = c
            .query_row("SELECT COUNT(*) FROM file_manifest", [], |r| r.get(0))
            .ok()?;
        let chunks: i64 = c
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .ok()?;
        Some((files, chunks))
    });

    match counts {
        Some((0, 0)) => format!("EMPTY — never ingested ({age})"),
        Some((files, chunks)) => {
            format!("{files} files, {chunks} chunks, indexed {age}")
        }
        // Present but unreadable/not an index: worth seeing, not worth guessing.
        None => format!("UNREADABLE ({} bytes, {age})", meta.len()),
    }
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
    // localhost/loopback and the bind address are always allowed. Any extra
    // reachable hostnames/IPs (e.g. the LAN address this server is served at)
    // come from config, not hardcoded, so no deploy-specific address lands in
    // the public repo: CODE_INDEX_ALLOWED_HOSTS (comma-separated) env override →
    // `[code_index].allowed_hosts` in ~/.config/agent/config.toml. For each
    // entry the bare host and `host:port` form are both allowed.
    let port = addr.rsplit(':').next().unwrap_or("7622");
    let mut allowed: Vec<String> = vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "::1".into(),
        addr.to_string(),
    ];
    let extra = std::env::var("CODE_INDEX_ALLOWED_HOSTS")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| code_index::embed::read_config_toml_value("code_index", "allowed_hosts"));
    if let Some(extra) = extra {
        for h in extra.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            allowed.push(h.to_string());
            allowed.push(format!("{h}:{port}"));
        }
    }
    config.allowed_hosts = allowed;
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
