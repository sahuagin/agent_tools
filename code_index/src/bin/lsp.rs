//! code-index-lsp: LSP server exposing code-index recall via workspace/symbol.
//!
//! Wraps the existing `code_index` library. No new analysis logic —
//! pure transport adapter from LSP to the recall/store API.
//!
//! Launch:  code-index-lsp [--db PATH]
//! Env:     CODE_INDEX_DB=path/to/index.db

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tower_lsp::jsonrpc::{self, Error};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use code_index::embed::select_embedder;
use code_index::recall::{self, RecallMode};
use code_index::store::SqliteStore;
use code_index::{Chunk, ChunkKind};

struct IndexHandle {
    db_path: tokio::sync::Mutex<PathBuf>,
}

impl IndexHandle {
    async fn db_path(&self) -> PathBuf {
        self.db_path.lock().await.clone()
    }

    fn recall_blocking(
        db_path: &Path,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(Chunk, f32)>> {
        let store = SqliteStore::open_at(db_path)?;
        let embedder = select_embedder();

        // Try hybrid first; fall back to lexical if embedding fails
        // (bad key, network error, model unavailable).
        let hits = recall::recall_with_mode(
            &store,
            embedder.as_ref(),
            query,
            limit,
            true,
            RecallMode::Hybrid,
        )
        .or_else(|_| {
            recall::recall_with_mode(
                &store,
                embedder.as_ref(),
                query,
                limit,
                true,
                RecallMode::Lexical,
            )
        })?;

        Ok(hits
            .into_iter()
            .filter_map(|h| h.chunk.map(|c| (c, h.score)))
            .collect())
    }
}

/// Minimum file size for a DB to be considered populated.
/// An empty code-index SQLite DB is ~73KB; a real index is 40MB+.
const MIN_POPULATED_DB_SIZE: u64 = 100_000;

fn db_is_populated(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.len() >= MIN_POPULATED_DB_SIZE)
}

/// Resolve the db path for a given rootUri. Checks:
/// 1. <root>/.code_index/index.db (per-project, only if populated)
/// 2. ~/.cache/code_index/<basename>.db (global cache, cron-maintained)
/// 3. Falls back to the server's default db_path
fn resolve_db_for_root(root_uri: Option<&str>, fallback: &Path) -> PathBuf {
    if let Some(uri) = root_uri {
        let root = uri.strip_prefix("file://").unwrap_or(uri);
        let root = PathBuf::from(root);

        // Per-project index (skip empty/stub DBs)
        let project_db = root.join(".code_index/index.db");
        if db_is_populated(&project_db) {
            return project_db;
        }

        // Global cache by basename
        if let Some(name) = root.file_name().and_then(|n| n.to_str()) {
            if let Some(home) = std::env::var_os("HOME") {
                let cache_db = PathBuf::from(home)
                    .join(".cache/code_index")
                    .join(format!("{name}.db"));
                if db_is_populated(&cache_db) {
                    return cache_db;
                }
            }
        }
    }
    fallback.to_owned()
}

#[derive(Clone)]
struct Backend {
    client: Client,
    index: Arc<IndexHandle>,
    default_db: PathBuf,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        let root_uri = params.root_uri.as_ref().map(|u| u.as_str());
        let db_path = resolve_db_for_root(root_uri, &self.default_db);
        // Swap the index handle to point at the resolved db.
        // Safe: each connection gets its own Backend clone.
        *self.index.db_path.lock().await = db_path.clone();
        self.client
            .log_message(
                MessageType::INFO,
                format!("code-index-lsp: using db {}", db_path.display()),
            )
            .await;
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                workspace_symbol_provider: Some(OneOf::Left(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec!["codeIndex.reindex".into()],
                    work_done_progress_options: Default::default(),
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "code-index-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let db = self.index.db_path().await;
        self.client
            .log_message(
                MessageType::INFO,
                format!("code-index-lsp ready (db: {})", db.display()),
            )
            .await;
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        Ok(())
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> jsonrpc::Result<Option<Vec<SymbolInformation>>> {
        let query = params.query;
        if query.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let db_path = self.index.db_path().await;
        let results =
            tokio::task::spawn_blocking(move || IndexHandle::recall_blocking(&db_path, &query, 20))
                .await
                .map_err(|_| Error::internal_error())?
                .map_err(|_| Error::internal_error())?;

        let cwd = std::env::current_dir().unwrap_or_default();
        let symbols: Vec<SymbolInformation> = results
            .into_iter()
            .filter_map(|(chunk, score)| {
                let abs_path = if chunk.file.is_relative() {
                    cwd.join(&chunk.file)
                } else {
                    chunk.file.clone()
                };
                let uri = Url::from_file_path(&abs_path).ok()?;
                #[allow(deprecated)]
                Some(SymbolInformation {
                    name: format!("{} ({:.3})", chunk.name, score),
                    kind: chunk_kind_to_symbol_kind(chunk.kind),
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri,
                        range: Range {
                            start: Position {
                                line: chunk.lines.start.saturating_sub(1) as u32,
                                character: 0,
                            },
                            end: Position {
                                line: chunk.lines.end.saturating_sub(1) as u32,
                                character: 0,
                            },
                        },
                    },
                    container_name: chunk.file.parent().map(|p| p.display().to_string()),
                })
            })
            .collect();

        Ok(Some(symbols))
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> jsonrpc::Result<Option<serde_json::Value>> {
        match params.command.as_str() {
            "codeIndex.reindex" => {
                self.client
                    .log_message(MessageType::INFO, "reindex requested (not yet implemented)")
                    .await;
                Ok(Some(serde_json::json!({"status": "not_implemented"})))
            }
            _ => Err(Error::method_not_found()),
        }
    }
}

fn chunk_kind_to_symbol_kind(kind: ChunkKind) -> SymbolKind {
    match kind {
        ChunkKind::Function => SymbolKind::FUNCTION,
        ChunkKind::Method => SymbolKind::METHOD,
        ChunkKind::Class => SymbolKind::CLASS,
        ChunkKind::Struct => SymbolKind::STRUCT,
        ChunkKind::Enum => SymbolKind::ENUM,
        ChunkKind::Trait => SymbolKind::INTERFACE,
        ChunkKind::Impl => SymbolKind::STRUCT,
        ChunkKind::Interface => SymbolKind::INTERFACE,
        ChunkKind::Type => SymbolKind::TYPE_PARAMETER,
        ChunkKind::Module => SymbolKind::MODULE,
        ChunkKind::Constant => SymbolKind::CONSTANT,
        ChunkKind::Macro => SymbolKind::FUNCTION,
        ChunkKind::Test => SymbolKind::FUNCTION,
        ChunkKind::Other => SymbolKind::VARIABLE,
    }
}

fn resolve_db() -> PathBuf {
    if let Some(p) = std::env::var_os("CODE_INDEX_DB") {
        return PathBuf::from(p);
    }
    // Try .code_index/index.db in cwd (only if populated)
    let cwd_db = PathBuf::from(".code_index/index.db");
    if db_is_populated(&cwd_db) {
        return cwd_db;
    }
    // Fallback to ~/.cache/code_index/ convention
    if let Some(home) = std::env::var_os("HOME") {
        let cache_db = PathBuf::from(home)
            .join(".cache")
            .join("code_index")
            .join("index.db");
        if db_is_populated(&cache_db) {
            return cache_db;
        }
    }
    cwd_db
}

#[tokio::main]
async fn main() {
    let db_path = resolve_db();

    let listen = std::env::args().nth(1);

    match listen.as_deref() {
        Some(addr) if addr.starts_with("--listen=") => {
            let addr = &addr["--listen=".len()..];
            serve_tcp(addr, db_path).await;
        }
        Some("--listen") => {
            let addr = std::env::args().nth(2).unwrap_or_else(|| {
                eprintln!("usage: code-index-lsp --listen <host:port>");
                std::process::exit(1);
            });
            serve_tcp(&addr, db_path).await;
        }
        _ => {
            // Default: stdio (editor-compatible)
            let db = db_path.clone();
            let (service, socket) = LspService::new(|client| Backend {
                client,
                index: Arc::new(IndexHandle {
                    db_path: tokio::sync::Mutex::new(db),
                }),
                default_db: db_path,
            });
            Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
                .serve(service)
                .await;
        }
    }
}

async fn serve_tcp(addr: &str, db_path: PathBuf) {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("failed to bind {addr}: {e}");
            std::process::exit(1);
        });
    eprintln!("code-index-lsp listening on {addr}");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        eprintln!("client connected: {peer}");
        let default = db_path.clone();
        tokio::spawn(async move {
            let (read, write) = tokio::io::split(stream);
            let db = default.clone();
            let (service, socket) = LspService::new(|client| Backend {
                client,
                index: Arc::new(IndexHandle {
                    db_path: tokio::sync::Mutex::new(db),
                }),
                default_db: default,
            });
            Server::new(read, write, socket).serve(service).await;
            eprintln!("client disconnected: {peer}");
        });
    }
}
