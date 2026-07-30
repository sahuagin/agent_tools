# LSP integration research for `code_index`

## Context

`code_index` is an existing Rust code-analysis tool. The reported architecture
is:

- tree-sitter parsing for Rust and Python
- SQLite storage via `rusqlite`
- embedding-based semantic search via `ureq` API calls
- `petgraph` call/dependency graphs
- CLI subcommands:
  - `ingest`
  - `recall`
  - `graph`

This document evaluates wrapping the existing analyzer as a Language Server
Protocol server, with the minimal useful goal of exposing semantic recall through
LSP.

> Source note: this research was prepared without direct filesystem access in
> the current worker session. Before implementation, verify exact module names,
> public APIs, database schema, and CLI/library split in
> `~/src/agent_tools/code_index/src/`.

---

## 1. Rust LSP server crates

The main Rust LSP server options are:

1. `tower-lsp`
2. `lsp-server`
3. `async-lsp`

### `tower-lsp`

Crate:

```toml
tower-lsp = "0.20"
```

Typical companions:

```toml
tokio = { version = "1", features = ["full"] }
anyhow = "1"
serde_json = "1"
```

`tower-lsp` is the highest-level option. It provides:

- async `LanguageServer` trait
- request/notification dispatch
- JSON-RPC transport over stdio
- typed LSP objects via `lsp-types`
- easy registration of core methods:
  - `initialize`
  - `initialized`
  - `shutdown`
  - `workspace/symbol`
  - `textDocument/hover`
  - `textDocument/definition`
  - custom requests can be handled through extension methods, though not as
    ergonomically as built-in methods

Example shape:

```rust
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::*,
    Client, LanguageServer, LspService, Server,
};

struct Backend {
    client: Client,
    index: CodeIndex,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                workspace_symbol_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "code-index-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn workspace_symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = params.query;
        let hits = self.index.recall(&query).await?;
        Ok(Some(hits.into_iter().map(hit_to_symbol).collect()))
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
```

`tower-lsp` is a good fit if `code_index` can be exposed as a library-like API
inside the same process.

Pros:

- Fastest path to a working server.
- Very small server skeleton.
- Good for wrapping an existing Rust library.
- Async-friendly if embedding calls or indexing need async orchestration.
- Commonly used for custom/small language servers.

Cons:

- Less control over the raw JSON-RPC loop than `lsp-server`.
- Custom LSP extensions are possible but less direct than with a fully manual
  dispatcher.
- Project maintenance cadence should be checked before committing.

### `lsp-server`

Crates:

```toml
lsp-server = "0.7"
lsp-types = "0.95"
```

`lsp-server` is the lower-level synchronous crate used by rust-analyzer. It
provides the transport and message plumbing but leaves dispatch, threading, and
server state management to the application.

Example shape:

```rust
use lsp_server::{Connection, Message, Request, Response};
use lsp_types::{InitializeParams, InitializeResult, ServerCapabilities};

fn main() -> anyhow::Result<()> {
    let (connection, io_threads) = Connection::stdio();

    let server_capabilities = serde_json::to_value(&ServerCapabilities {
        workspace_symbol_provider: Some(lsp_types::OneOf::Left(true)),
        ..Default::default()
    })?;

    let initialization_params = connection.initialize(server_capabilities)?;
    let _params: InitializeParams = serde_json::from_value(initialization_params)?;

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    break;
                }

                match req.method.as_str() {
                    "workspace/symbol" => {
                        let response = handle_workspace_symbol(req)?;
                        connection.sender.send(Message::Response(response))?;
                    }
                    _ => {
                        connection.sender.send(Message::Response(Response {
                            id: req.id,
                            result: None,
                            error: Some(lsp_server::ResponseError {
                                code: lsp_server::ErrorCode::MethodNotFound as i32,
                                message: format!("unknown request {}", req.method),
                                data: None,
                            }),
                        }))?;
                    }
                }
            }
            Message::Notification(n) => {
                // handle didOpen/didChange/didSave/etc
            }
            Message::Response(_) => {}
        }
    }

    io_threads.join()?;
    Ok(())
}
```

Pros:

- Battle-tested by rust-analyzer.
- Full control over dispatch, threading, cancellation, progress, custom methods,
  and unusual protocol behavior.
- Easy to implement custom requests by matching method strings.

Cons:

- More boilerplate.
- You own concurrency.
- You own request cancellation behavior.
- More work than needed for a thin wrapper.

This fits best if `code_index` eventually needs rust-analyzer-like complexity:
background indexing, snapshots, cancellation, multi-root workspaces, and custom
protocol extensions.

### `async-lsp`

Crate:

```toml
async-lsp = "0.2"
```

`async-lsp` is a newer async framework for LSP servers. It is built around
service composition and async dispatch.

Pros:

- Modern async design.
- Composable service/middleware style.
- Good fit for a server that wants structured async request handling.

Cons:

- Smaller ecosystem and fewer examples than `tower-lsp`.
- More conceptual overhead than `tower-lsp`.
- Less obvious as the shortest path for a small wrapper.

### Recommendation

Use `tower-lsp` for the first implementation.

Reason:

`code_index` already has the interesting functionality: parsing, indexing,
semantic recall, symbol metadata, and graphs. The LSP server should initially be
a thin adapter, not a new analysis engine. `tower-lsp` gives the shortest path
from existing library calls to LSP methods.

Use `lsp-server` later only if one of these becomes necessary:

- custom protocol surface becomes large
- precise cancellation is needed
- request scheduling needs rust-analyzer-style control
- multi-workspace live indexing becomes complex
- Claude Code / MCP bridge needs non-standard JSON-RPC behavior

---

## 2. LSP methods that map to `code_index`

### `workspace/symbol`

LSP method:

```text
workspace/symbol
```

Rust type:

```rust
WorkspaceSymbolParams {
    query: String,
    work_done_progress_params: WorkDoneProgressParams,
    partial_result_params: PartialResultParams,
}
```

This is the best first mapping for semantic recall.

Traditional LSP meaning:

> Search for symbols in the workspace whose names match the query.

`code_index` can provide a richer interpretation:

> Search the indexed codebase semantically and return symbol-like hits.

Possible mapping:

| LSP field | `code_index` source |
|---|---|
| `query` | recall query string |
| result item name | symbol/function/class/module name |
| kind | tree-sitter symbol kind mapping |
| location URI | file path |
| location range | symbol span from parser/index |
| container name | module/class/parent symbol |
| tags | deprecated? optional |
| score | not directly supported; encode in detail/name or custom data if using newer LSP types |

For LSP compatibility, avoid inventing semantics in mandatory fields. Good result
names:

```text
parse_user_config
UserRepository.find_by_email
src/agent/loop.rs: assemble_context
```

If score is useful, include it in the display name or use the newer
`WorkspaceSymbol` form with `data`, depending on client support.

Example conversion:

```rust
fn hit_to_symbol(hit: RecallHit) -> SymbolInformation {
    SymbolInformation {
        name: format!("{}  ({:.3})", hit.symbol_name, hit.score),
        kind: symbol_kind(hit.kind),
        tags: None,
        deprecated: None,
        location: Location {
            uri: Url::from_file_path(&hit.path).unwrap(),
            range: Range {
                start: Position {
                    line: hit.start_line.saturating_sub(1),
                    character: hit.start_col,
                },
                end: Position {
                    line: hit.end_line.saturating_sub(1),
                    character: hit.end_col,
                },
            },
        },
        container_name: hit.container,
    }
}
```

Caveat:

Some clients expect `workspace/symbol` to be fast and name-oriented. Semantic
embedding search may be slower. For Claude Code usage, that may be acceptable.
For editor usage, add a limit and maybe debounce/cache queries.

Recommended default:

- `workspace/symbol` → semantic recall
- limit default: 20 or 50
- include score in label
- prefer indexed symbols over arbitrary chunks if possible

### `textDocument/hover`

LSP method:

```text
textDocument/hover
```

This can expose indexed symbol information at a position.

Possible output:

- symbol name
- kind
- defining file/range
- docstring/comments if indexed
- signature if available
- language
- semantic summary if embeddings store one
- inbound/outbound graph summary:
  - callers
  - callees
  - imports/dependencies

Example hover markdown:

```markdown
```rust
fn build_call_graph(...)
```

**code_index**

- kind: function
- defined: `src/graph.rs:42`
- calls: 8 symbols
- called by: 3 symbols
- embedding: indexed

Top related symbols:

1. `GraphStore::insert_edge` — 0.84
2. `resolve_symbol_edges` — 0.79
```

Implementation requirement:

The server must map LSP position to an indexed symbol. That requires either:

1. exact symbol ranges in SQLite, or
2. token lookup using tree-sitter on the current file, then DB lookup by
   name/file/range.

Minimal hover can be DB-only if symbol ranges are stored.

### `textDocument/definition`

LSP method:

```text
textDocument/definition
```

Potential mappings:

1. If the cursor is on a symbol reference:
   - return the indexed definition location.
2. If graph edges include reference-to-definition edges:
   - return the target node location.
3. If only call/dependency edges exist:
   - return best matching definition by name and file context.

This is only as good as the index schema. Tree-sitter alone gives definitions
well; references are harder.

Recommended implementation order:

1. Implement definition for symbols whose exact range is indexed.
2. Add reference resolution later.
3. If ambiguous, return multiple `Location`s.

### `textDocument/references`

If graph data includes inbound edges or reference locations, expose:

```text
textDocument/references
```

Mappings:

- callers of a function
- references to a class/function/module
- import users

This may actually be more natural than `definition` for graph data.

### `callHierarchy/*`

LSP has native call hierarchy support:

```text
textDocument/prepareCallHierarchy
callHierarchy/incomingCalls
callHierarchy/outgoingCalls
```

This maps directly to `petgraph` call graph data.

Potential mapping:

| LSP method | `code_index` capability |
|---|---|
| `prepareCallHierarchy` | find symbol at cursor |
| `incomingCalls` | graph inbound edges |
| `outgoingCalls` | graph outbound edges |

This is more semantically correct than overloading `definition` for graph edges.

### `workspace/executeCommand`

Useful for custom operations while staying inside standard LSP.

Examples:

```text
codeIndex.recall
codeIndex.reindexWorkspace
codeIndex.reindexFile
codeIndex.relatedSymbols
codeIndex.graphNeighbors
```

Advertise:

```rust
execute_command_provider: Some(ExecuteCommandOptions {
    commands: vec![
        "codeIndex.recall".into(),
        "codeIndex.reindexWorkspace".into(),
        "codeIndex.reindexFile".into(),
        "codeIndex.relatedSymbols".into(),
    ],
    work_done_progress_options: Default::default(),
})
```

### Custom methods

For Claude Code or custom clients, define explicit custom JSON-RPC methods:

```text
codeIndex/semanticSearch
codeIndex/relatedSymbols
codeIndex/graphNeighbors
codeIndex/reindex
```

Example request:

```json
{
  "query": "where do we assemble prompt context?",
  "limit": 20,
  "language": "rust"
}
```

Example response:

```json
{
  "hits": [
    {
      "score": 0.842,
      "symbol": "assemble_context",
      "kind": "function",
      "uri": "file:///repo/src/context.rs",
      "range": {
        "start": { "line": 41, "character": 0 },
        "end": { "line": 92, "character": 1 }
      },
      "snippet": "fn assemble_context(...) { ... }"
    }
  ]
}
```

Recommendation:

- Use `workspace/symbol` for broad compatibility.
- Add `workspace/executeCommand` for custom commands.
- Add true custom methods only if Claude Code bridge/client can call them.

---

## 3. Minimal viable LSP server

### Goal

Expose `code_index recall` as `workspace/symbol`.

No live indexing. No hover. No definitions. No didChange. The server starts,
opens the existing SQLite index, receives semantic queries, returns locations.

### Expected scope

Estimated LOC:

| Component | LOC |
|---|---:|
| `src/bin/code-index-lsp.rs` server bootstrap | 40-70 |
| LSP backend struct and `LanguageServer` impl | 120-180 |
| adapter from recall hits to LSP symbols | 60-100 |
| config/env/db path loading | 30-60 |
| error handling/logging | 30-60 |
| total | 250-450 |

If `code_index` is currently CLI-only and does not expose a library API, add:

| Component | LOC |
|---|---:|
| refactor recall logic into callable library function | 100-250 |
| shared config type | 50-100 |

Total with small refactor: 400-800 LOC.

### Complexity

Low to moderate.

Main risk is not LSP. Main risk is whether `recall` is currently implemented as
CLI code with side effects/printing rather than a reusable function returning
structured hits.

The desired internal API is something like:

```rust
pub struct RecallQuery<'a> {
    pub query: &'a str,
    pub limit: usize,
    pub language: Option<&'a str>,
}

pub struct RecallHit {
    pub score: f32,
    pub symbol_name: String,
    pub symbol_kind: String,
    pub path: PathBuf,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub container: Option<String>,
    pub snippet: Option<String>,
}

pub trait RecallIndex {
    fn recall(&self, query: RecallQuery<'_>) -> anyhow::Result<Vec<RecallHit>>;
}
```

If embedding recall makes blocking HTTP calls through `ureq`, then inside
`tower-lsp` either:

1. keep the handler blocking for MVP, or
2. wrap recall in `tokio::task::spawn_blocking`.

MVP recommendation:

```rust
async fn workspace_symbol(
    &self,
    params: WorkspaceSymbolParams,
) -> Result<Option<Vec<SymbolInformation>>> {
    let query = params.query;
    let index = self.index.clone();

    let hits = tokio::task::spawn_blocking(move || {
        index.recall_blocking(&query, 20)
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;

    Ok(Some(hits.into_iter().map(hit_to_symbol).collect()))
}
```

This avoids blocking the async runtime if recall performs SQLite and HTTP work.

### Sketch Cargo changes

```toml
[dependencies]
anyhow = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
tower-lsp = "0.20"
url = "2"

# existing dependencies likely already present:
rusqlite = { version = "...", features = ["bundled"] }
ureq = "..."
tree-sitter = "..."
petgraph = "..."
```

### Sketch `code-index-lsp.rs`

```rust
use std::{path::PathBuf, sync::Arc};

use tower_lsp::{
    jsonrpc::{Error, Result},
    lsp_types::*,
    Client, LanguageServer, LspService, Server,
};

#[derive(Clone)]
struct Backend {
    client: Client,
    index: Arc<CodeIndexHandle>,
}

struct CodeIndexHandle {
    db_path: PathBuf,
}

impl CodeIndexHandle {
    fn recall_blocking(&self, query: &str, limit: usize) -> anyhow::Result<Vec<RecallHit>> {
        // TODO: call existing code_index recall library function.
        //
        // Desired shape:
        // code_index::recall::recall(&self.db_path, query, limit)
        todo!()
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                workspace_symbol_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        "codeIndex.recall".into(),
                        "codeIndex.reindexWorkspace".into(),
                    ],
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
        self.client
            .log_message(MessageType::INFO, "code-index-lsp initialized")
            .await;
    }

    async fn workspace_symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = params.query;
        let index = self.index.clone();

        let hits = tokio::task::spawn_blocking(move || {
            index.recall_blocking(&query, 20)
        })
        .await
        .map_err(|err| Error::internal_error(format!("join error: {err}")))?
        .map_err(|err| Error::internal_error(format!("recall error: {err}")))?;

        Ok(Some(hits.into_iter().map(hit_to_symbol).collect()))
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

fn hit_to_symbol(hit: RecallHit) -> SymbolInformation {
    SymbolInformation {
        name: format!("{}  ({:.3})", hit.symbol_name, hit.score),
        kind: symbol_kind(&hit.symbol_kind),
        tags: None,
        deprecated: None,
        location: Location {
            uri: Url::from_file_path(&hit.path)
                .expect("indexed paths should be absolute"),
            range: Range {
                start: Position {
                    line: hit.start_line.saturating_sub(1),
                    character: hit.start_col,
                },
                end: Position {
                    line: hit.end_line.saturating_sub(1),
                    character: hit.end_col,
                },
            },
        },
        container_name: hit.container,
    }
}

fn symbol_kind(kind: &str) -> SymbolKind {
    match kind {
        "function" | "method" => SymbolKind::FUNCTION,
        "class" | "struct" => SymbolKind::STRUCT,
        "module" => SymbolKind::MODULE,
        "constant" => SymbolKind::CONSTANT,
        "field" => SymbolKind::FIELD,
        "variable" => SymbolKind::VARIABLE,
        _ => SymbolKind::OBJECT,
    }
}

#[tokio::main]
async fn main() {
    let db_path = std::env::var_os("CODE_INDEX_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".code_index/index.sqlite"));

    let (service, socket) = LspService::new(|client| Backend {
        client,
        index: Arc::new(CodeIndexHandle { db_path }),
    });

    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;
}
```

### MVP behavior

From a client:

```text
workspace/symbol query="where is prompt context assembled?"
```

Returns:

```text
assemble_context  (0.842)
build_context_rope  (0.801)
PromptAssembler::append_tool_results  (0.755)
```

Each result has a file URI and range, so clients can jump to source.

---

## 4. Claude Code consumption

### Important distinction

Claude Code itself is not a general editor with native LSP configuration in the
same way as Neovim or VS Code. The common pattern is to bridge LSP servers into
Claude Code through MCP or a Claude Code extension/helper.

The referenced resources are relevant here:

- <https://karanbansal.in/blog/claude-code-lsp/>
- <https://github.com/Piebald-AI/claude-code-lsps>

The pattern from `claude-code-lsps` is:

1. run one or more language servers as subprocesses
2. talk LSP over stdio
3. expose LSP-backed operations to Claude Code as tools
4. configure the bridge as an MCP server in Claude Code

So Claude Code does not need to know that `code-index-lsp` is semantic-search
backed. It just sees tools exposed by the bridge.

### Expected configuration shape

A typical Claude Code MCP config shape is:

```json
{
  "mcpServers": {
    "lsps": {
      "command": "uvx",
      "args": [
        "claude-code-lsps"
      ],
      "env": {
        "LSP_CONFIG": "/path/to/lsp-config.json"
      }
    }
  }
}
```

Or, depending on the installed bridge:

```json
{
  "mcpServers": {
    "claude-code-lsps": {
      "command": "claude-code-lsps",
      "args": [
        "--config",
        "~/src/agent_tools/code_index/lsp-config.json"
      ]
    }
  }
}
```

The LSP bridge config would then include `code-index-lsp` as a server:

```json
{
  "servers": {
    "code-index": {
      "command": "~/src/agent_tools/code_index/target/release/code-index-lsp",
      "args": [],
      "env": {
        "CODE_INDEX_DB": "~/src/agent_tools/code_index/.code_index/index.sqlite"
      },
      "languages": ["rust", "python"],
      "rootPatterns": [".git", "Cargo.toml", "pyproject.toml"]
    }
  }
}
```

Exact field names depend on the bridge. Verify against
`Piebald-AI/claude-code-lsps`.

### What Claude gets

If the bridge exposes `workspace/symbol`, Claude can ask:

- “Find symbols related to session compaction.”
- “Where is semantic recall implemented?”
- “Find the code that builds dependency graphs.”

Under the hood:

```text
Claude tool call → MCP bridge → workspace/symbol → code-index-lsp → recall
```

The returned locations become navigable references for Claude.

### Better Claude-facing tool names

`workspace/symbol` is an LSP method name, but it is not the best tool name for
Claude. The bridge or custom MCP layer should expose friendlier tools:

```text
semantic_search_code
lookup_symbol
find_related_symbols
show_callers
show_callees
```

If using only generic `claude-code-lsps`, the method may appear as an LSP-ish
tool. That is acceptable for first integration, but a custom MCP wrapper around
`code_index` might eventually be better than LSP for Claude-specific workflows.

### LSP vs MCP for Claude

LSP is good when:

- the same server should be usable by editors and Claude Code
- jump-to-definition, hover, and workspace symbols are useful
- existing LSP bridge tooling can be reused

MCP is better when:

- the main consumer is Claude
- methods are semantic/search-oriented rather than editor-position-oriented
- custom return schemas matter
- you want explicit tools like `semantic_search_code`

Recommendation:

Build LSP first only if editor integration is also desired or the
`claude-code-lsps` bridge is already the intended deployment path.

If Claude Code is the only consumer, a direct MCP server wrapping `code_index`
may be simpler and more semantically honest than translating through LSP.

---

## 5. Keeping the index live

### Does `tower-lsp` support file watching natively?

No, not in the sense of watching files itself.

`tower-lsp` supports the LSP protocol methods and client capability registration.
File watching in LSP usually works in one of two ways:

1. Client sends document lifecycle notifications:
   - `textDocument/didOpen`
   - `textDocument/didChange`
   - `textDocument/didSave`
   - `textDocument/didClose`

2. Server asks client to watch files via dynamic registration:
   - `client/registerCapability`
   - method: `workspace/didChangeWatchedFiles`
   - client then sends `workspace/didChangeWatchedFiles` notifications

`tower-lsp` can receive these notifications and can send registration requests,
but it does not itself monitor the filesystem.

If the server wants independent filesystem watching, use the `notify` crate.

Crate:

```toml
notify = "6"
```

or current latest:

```toml
notify = "7"
```

Verify latest compatible version before implementation.

### Option A: client-driven updates

Advertise:

```rust
text_document_sync: Some(TextDocumentSyncCapability::Kind(
    TextDocumentSyncKind::INCREMENTAL,
)),
```

Implement:

```rust
async fn did_open(&self, params: DidOpenTextDocumentParams) {
    // optionally index opened file
}

async fn did_change(&self, params: DidChangeTextDocumentParams) {
    // update in-memory overlay or mark dirty
}

async fn did_save(&self, params: DidSaveTextDocumentParams) {
    // re-index saved file
}
```

Pros:

- Simple.
- Works well for editor-driven use.
- Avoids indexing partially-written files unless using didChange.

Cons:

- Does not catch changes made outside the LSP client unless file watching is
  also configured.
- Claude Code may not behave like an editor and may not send full document
  lifecycle events.

Recommended for editor integration, but not sufficient alone for Claude Code.

### Option B: LSP watched files

During `initialized`, dynamically register:

```rust
use tower_lsp::lsp_types::*;

async fn initialized(&self, _: InitializedParams) {
    let registration = Registration {
        id: "code-index-watch-files".to_string(),
        method: "workspace/didChangeWatchedFiles".to_string(),
        register_options: Some(serde_json::json!({
            "watchers": [
                { "globPattern": "**/*.rs" },
                { "globPattern": "**/*.py" },
                { "globPattern": "**/Cargo.toml" },
                { "globPattern": "**/pyproject.toml" }
            ]
        })),
    };

    let _ = self.client
        .register_capability(vec![registration])
        .await;
}
```

Then implement:

```rust
async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
    for change in params.changes {
        match change.typ {
            FileChangeType::CREATED | FileChangeType::CHANGED => {
                self.queue_reindex(change.uri).await;
            }
            FileChangeType::DELETED => {
                self.queue_remove(change.uri).await;
            }
            _ => {}
        }
    }
}
```

Pros:

- LSP-native.
- Client handles filesystem watching.

Cons:

- Dynamic registration support varies by client.
- Claude Code LSP bridges may or may not forward watched-file events.
- Still need debouncing and indexing queue.

### Option C: server-side file watching with `notify`

Run a background watcher inside the LSP server:

```rust
let (tx, mut rx) = tokio::sync::mpsc::channel(1024);

std::thread::spawn(move || {
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.blocking_send(res);
    })?;

    watcher.watch(root.as_path(), notify::RecursiveMode::Recursive)?;

    loop {
        std::thread::park();
    }
});
```

Then consume events:

```rust
tokio::spawn(async move {
    while let Some(event) = rx.recv().await {
        indexer.enqueue(event).await;
    }
});
```

Pros:

- Works even if the client does not send watched-file notifications.
- Better for Claude Code and other non-editor clients.
- Keeps the index live for shell edits, git checkouts, codegen, etc.

Cons:

- More server responsibility.
- Need ignore rules:
  - `.git`
  - `target`
  - `.venv`
  - `node_modules`
  - generated artifacts
- Need debouncing.
- Need careful SQLite write serialization.

### Recommended live-index design

Use a background indexing queue.

Core pieces:

```rust
struct IndexUpdate {
    path: PathBuf,
    kind: IndexUpdateKind,
}

enum IndexUpdateKind {
    Upsert,
    Delete,
}

struct Indexer {
    tx: tokio::sync::mpsc::Sender<IndexUpdate>,
}
```

Properties:

- debounce by path for 250-1000ms
- batch SQLite writes
- serialize writes through one worker
- allow concurrent reads if SQLite is in WAL mode
- re-embed only changed symbols/chunks
- remove deleted file records before re-inserting
- expose indexing status through:
  - `window/logMessage`
  - custom command `codeIndex.status`
  - optionally LSP progress tokens

SQLite settings to consider:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
```

Live-index phases:

1. MVP:
   - no live updates
   - user runs `code_index ingest`
   - LSP serves existing DB

2. Phase 2:
   - reindex on `didSave`
   - enough for editor users

3. Phase 3:
   - server-side `notify` watcher
   - debounce and batch
   - enough for Claude Code and shell-driven edits

4. Phase 4:
   - incremental parsing and symbol-level updates
   - avoid full-file re-embedding where possible

---

## Recommended implementation plan

### Phase 0: Make recall callable

Before LSP, ensure the CLI recall path is factored into a reusable API.

Desired module shape:

```rust
pub mod index;
pub mod recall;
pub mod symbols;
pub mod graph;

pub use recall::{RecallHit, RecallOptions, RecallEngine};
```

CLI should become a thin wrapper:

```rust
let hits = RecallEngine::open(config.db_path)?.recall(options)?;
print_hits(hits);
```

This is the key enabling step.

### Phase 1: `code-index-lsp` binary with `workspace/symbol`

Add a new binary target:

```toml
[[bin]]
name = "code-index-lsp"
path = "src/bin/code-index-lsp.rs"
```

Capabilities:

- `initialize`
- `shutdown`
- `workspace/symbol`

No live updates.

Config:

- `CODE_INDEX_DB`
- optional `CODE_INDEX_ROOT`
- optional `CODE_INDEX_LIMIT`

### Phase 2: Hover

Add:

```rust
hover_provider: Some(HoverProviderCapability::Simple(true))
```

Implement:

```text
textDocument/hover
```

Use indexed symbol ranges to find symbol under cursor.

### Phase 3: Definition and references

Add:

```rust
definition_provider: Some(OneOf::Left(true)),
references_provider: Some(OneOf::Left(true)),
```

Implement:

- definition by symbol identity/name/range
- references by graph/reference records if available

### Phase 4: Call hierarchy

Add:

```rust
call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true))
```

Implement:

- `prepareCallHierarchy`
- `incomingCalls`
- `outgoingCalls`

This is the cleanest LSP projection of `petgraph` call data.

### Phase 5: Live updates

Start with:

- `didSave` reindex

Then add:

- server-side `notify` watcher
- debounced queue
- SQLite WAL mode
- progress/logging

---

## Final recommendation

Build the first LSP wrapper with `tower-lsp`.

The minimal useful server is small: roughly 250-450 LOC if recall is already
callable as a library API, or 400-800 LOC if the CLI needs a small refactor.

Expose semantic recall as `workspace/symbol` first. That gives immediate value
to editors and to Claude Code through an LSP bridge such as
`claude-code-lsps`.

Do not start with live indexing. For v0, require users to run:

```bash
code_index ingest /path/to/repo
CODE_INDEX_DB=/path/to/index.sqlite code-index-lsp
```

Then add `didSave` and/or `notify` once the LSP adapter is proven useful.

If Claude Code is the only real consumer, consider a direct MCP server after the
LSP experiment. LSP is excellent for editor-shaped operations; MCP is a better
native shape for semantic tools like:

```text
semantic_search_code
find_related_symbols
show_call_graph
reindex_file
```

The best near-term path is therefore:

1. factor `recall` into a reusable library function if needed
2. add `code-index-lsp` with `tower-lsp`
3. implement `workspace/symbol → recall`
4. connect it to Claude Code via `claude-code-lsps`
5. add hover/definition/call hierarchy only after the recall path is working
