# Spec: mu daemon as LSP client for code-index

## Summary

mu's daemon (`mu serve`) connects to `code-index-lsp` as an LSP client,
the same way an editor would. The LSP server runs independently (its own
process, its own lifecycle). The daemon connects over TCP or unix socket,
sends `workspace/symbol` queries, and materializes results as rope spans.

## Why not stdio

stdio is fine for a single-client model (one editor, one LSP server).
But mu's daemon may want multiple LSP connections (code-index, rust-analyzer,
a future docs-index), and multiple mu daemons may want to share one LSP
server. TCP or unix socket gives:

- **Multi-client**: multiple daemons connect to the same LSP server
- **Independent lifecycle**: LSP server stays up across daemon restarts
- **Discoverability**: the server advertises its socket path or port,
  composing with the service-discovery model from mu-037

## Architecture

```
code-index-lsp (standalone process)
    listens on: unix:~/.local/share/code-index/lsp.sock
    OR         tcp:127.0.0.1:7621

mu serve (daemon)
    ├── LspClient (tower-lsp-client or raw JSON-RPC over socket)
    │   ├── connect() → handshake → capabilities
    │   ├── workspace_symbol(query) → Vec<SymbolInfo>
    │   └── shutdown()
    ├── Agent loop
    │   ├── Tool: "index_recall" registered when LSP is connected
    │   └── Tool call → LspClient.workspace_symbol → results → rope spans
    └── Rope
        └── SpanKind::IndexRecall spans with chunk data
```

## LSP server changes (code-index-lsp)

### Listen mode

Add `--listen` flag to support socket connections alongside stdio:

```sh
# stdio (default, editor-compatible)
code-index-lsp

# Unix socket
code-index-lsp --listen unix:~/.local/share/code-index/lsp.sock

# TCP
code-index-lsp --listen tcp:127.0.0.1:7621
```

`tower-lsp` supports custom transports — instead of
`Server::new(stdin, stdout, socket)`, use a TCP/unix listener that
accepts connections and runs the LspService per connection.

### Multi-client considerations

Each connection gets its own `LanguageServer` instance sharing the
same `Arc<IndexHandle>` (the SQLite store). SQLite WAL mode handles
concurrent reads. The `reindex` command acquires a write lock.

### Service registration (future)

When `--listen` is active, optionally write a registration file:

```toml
# ~/.local/share/code-index/server.toml
pid = 12345
socket = "/home/tcovert/.local/share/code-index/lsp.sock"
db = "/home/tcovert/.cache/code_index/mu.db"
started_at = "2026-05-25T20:00:00Z"
capabilities = ["workspace/symbol", "codeIndex.reindex"]
```

mu's daemon discovers this file and connects automatically.

## mu daemon changes (mu-core + mu-coding)

### New module: `mu-core/src/lsp_client.rs`

Minimal LSP client — we only need the client half, not a full editor:

```rust
pub struct LspClient {
    transport: LspTransport,
    next_id: AtomicU64,
}

enum LspTransport {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl LspClient {
    pub async fn connect(addr: &str) -> Result<Self>;
    pub async fn initialize(&self) -> Result<ServerCapabilities>;
    pub async fn workspace_symbol(&self, query: &str) -> Result<Vec<SymbolInformation>>;
    pub async fn shutdown(&self) -> Result<()>;
}
```

Implementation: raw JSON-RPC over the socket. We don't need
`tower-lsp`'s client machinery — it's a handful of
`send_request` / `recv_response` methods over a framed stream.
`tokio::io::AsyncRead + AsyncWrite` with `Content-Length` framing.

### New tool: `index_recall`

Registered in the agent's tool set when an LSP connection is active:

```rust
ToolSpec {
    name: "index_recall",
    description: "Search the code index for symbols, functions, types, \
                  and concepts. Returns ranked results with file locations.",
    input_schema: json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Natural language or symbol name to search for"
            },
            "limit": {
                "type": "integer",
                "description": "Maximum results (default 10)",
                "default": 10
            }
        },
        "required": ["query"]
    }),
}
```

The model sees `index_recall` alongside `Read`, `Edit`, `Bash`, etc.
It doesn't know an LSP is behind it.

### Tool execution

```rust
async fn execute_index_recall(
    lsp: &LspClient,
    args: &Value,
) -> Result<String> {
    let query = args["query"].as_str().unwrap_or("");
    let limit = args["limit"].as_u64().unwrap_or(10) as usize;

    let symbols = lsp.workspace_symbol(query).await?;
    let symbols = &symbols[..symbols.len().min(limit)];

    // Format as text the model can consume
    let mut out = String::new();
    for sym in symbols {
        let path = sym.location.uri.path();
        let line = sym.location.range.start.line + 1;
        out.push_str(&format!(
            "{} ({:?}) at {}:{}\n",
            sym.name, sym.kind, path, line
        ));
    }
    Ok(out)
}
```

### Rope integration (optional, Phase 2)

Instead of returning results as tool output text, materialize them
as rope spans:

```rust
SpanKind::IndexRecall  // new variant

Span::new(
    format!("index-recall:{query_hash}"),
    SpanKind::IndexRecall,
    formatted_results,
    RetentionClass::Warm,  // eligible for eviction
)
```

This lets the context assembly system manage index results the same
way it manages memory recalls — they stay in context for the current
arc, get evicted when the rope needs space.

Phase 1 just returns tool output text. Phase 2 adds the rope span
path for persistent context.

## Connection lifecycle

### Startup

1. Daemon checks for `~/.local/share/code-index/server.toml`
2. If present → connect to the advertised socket
3. If absent → optionally spawn `code-index-lsp --listen ...` as a
   child process (like how editors auto-start language servers)
4. Send `initialize` → receive capabilities
5. If `workspaceSymbolProvider` is true → register `index_recall` tool
6. Send `initialized` notification

### Runtime

- Tool calls to `index_recall` → `workspace/symbol` to LSP
- Future: `textDocument/hover` for deeper symbol info
- Future: `codeIndex.reindex` when files change

### Shutdown

- Daemon closing → send `shutdown` + `exit` to LSP
- If daemon spawned the LSP → wait for process exit
- If LSP was pre-existing → just close the connection

### Reconnection

- If the LSP connection drops → deregister `index_recall` tool
- Periodically check for `server.toml` reappearing
- On reconnect → re-register tool

## Phase plan

### Phase 1: Tool-based integration

- Add `--listen` to code-index-lsp (TCP or unix socket)
- Write `mu-core/src/lsp_client.rs` (minimal JSON-RPC client)
- Register `index_recall` tool when connected
- Model uses it like any other tool

### Phase 2: Rope integration

- `SpanKind::IndexRecall` variant
- Results become spans in the rope
- Context assembly manages their lifetime
- Model sees index results as ambient context, not tool output

### Phase 3: Multi-LSP

- Connect to rust-analyzer alongside code-index-lsp
- Each LSP contributes its capabilities as tools
- Capability negotiation at connect time (the IMAP CAPABILITY
  pattern discussed 2026-05-25)

### Phase 4: Service discovery

- LSP servers register in the discovery layer (mu-037 Phase 2)
- Daemon auto-discovers and connects
- Same discovery surface for LSP servers, mu peers, and MCP servers

## Estimated effort

- Phase 1: ~400 LOC (listen mode + client + tool registration)
- Phase 2: ~100 LOC (new SpanKind + assembly integration)
- Phase 3: design work (trait abstraction for multi-LSP)
- Phase 4: depends on mu-037 Phase 2

## Risks

- JSON-RPC framing over raw sockets needs careful implementation
  (Content-Length headers, partial reads, async buffering)
- SQLite concurrent access from multiple daemon connections needs
  WAL mode (already the default for code-index)
- The `index_recall` tool adds to the tool count the model sees —
  keep the description concise per the FastMCP talk's "50 tools
  per agent" ceiling
