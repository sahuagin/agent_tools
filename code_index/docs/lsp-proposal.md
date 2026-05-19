# code-index-lsp: LSP server wrapping code-index

## Summary

Wrap the existing `code_index` library as an LSP server using `tower-lsp`.
The MVP exposes semantic+lexical recall via `workspace/symbol` — one
command, one binary, immediately useful for Claude Code and editors.

## Motivation

code-index exists as a CLI tool. It works, but:
- Agents forget to use it (activation energy)
- No file-change watching (index goes stale)
- No editor integration (no jump-to-definition from results)

An LSP server is always-on: editors and Claude Code query it automatically
via standard protocol. Same data, zero friction.

## Architecture

```
code-index-lsp (tower-lsp binary)
    ├── LanguageServer trait impl
    │   ├── workspace/symbol → recall (semantic + lexical fusion)
    │   ├── textDocument/hover → chunk info for symbol at cursor
    │   └── textDocument/definition → graph edge traversal
    ├── CodeIndexHandle (wraps code_index::store::SqliteStore)
    │   ├── recall_blocking() → tokio::spawn_blocking
    │   └── reindex() → code_index::ingest pipeline
    └── File watcher (notify crate, future)
        └── on file change → mark stale, re-chunk on next query
```

## MVP (Phase 1)

### Binary

New binary target in `~/src/agent_tools/code_index/`:

```toml
[[bin]]
name = "code-index-lsp"
path = "src/bin/lsp.rs"
```

### Dependencies to add

```toml
tower-lsp = "0.20"
tokio = { version = "1", features = ["full"] }
```

### Capabilities

| LSP method | code-index function | Phase |
|---|---|---|
| `workspace/symbol` | `recall::recall()` (semantic+lexical RRF) | 1 |
| `textDocument/hover` | `store.get_chunk()` for symbol at cursor | 2 |
| `textDocument/definition` | `store.find_chunks_by_name()` + edges | 2 |
| `workspace/executeCommand` `codeIndex.reindex` | `ingest::ingest()` | 1 |

### workspace/symbol mapping

Query comes in as a string. We:
1. Run `recall::recall()` with the query (already does RRF fusion)
2. Map each `Chunk` result to `SymbolInformation`:
   - `name` = `chunk.name` + score
   - `kind` = `chunk.kind` → `SymbolKind` mapping
   - `location` = `chunk.file` + `chunk.lines` → URI + Range

### Configuration

```sh
# DB path (auto-discovered or explicit)
CODE_INDEX_DB=.code_index/index.db

# Launch
code-index-lsp
# Reads DB from CWD or CODE_INDEX_DB env var
```

### Claude Code integration

In `.claude/settings.json`:

```json
{
  "mcpServers": {
    "code-index": {
      "command": "code-index-lsp",
      "args": [],
      "env": {
        "CODE_INDEX_DB": ".code_index/index.db"
      }
    }
  }
}
```

Or via the Piebald-style LSP bridge if Claude Code doesn't speak LSP
natively for custom servers.

## Phase 2 (after MVP)

- `textDocument/hover` — show chunk text, kind, file, containing module
- `textDocument/definition` — follow `Calls`/`References` edges in the graph
- File watching via `notify` — re-chunk changed files automatically
- `textDocument/didSave` — trigger incremental re-index on save

## Phase 3 (after code-index epic)

When code-index gains typed artifact indexing (specs, beads, commits):
- `workspace/symbol` returns specs/beads alongside code
- Results labeled with artifact type
- Filter by `--kind` equivalent via custom LSP parameters

## Acceptance criteria (MVP)

1. `cargo build --release -p code_index --bin code-index-lsp` produces binary
2. Binary launches, speaks LSP over stdio
3. `workspace/symbol` with a query returns ranked results from the index
4. `workspace/executeCommand` `codeIndex.reindex` triggers re-ingest
5. Results include file URI + line range (editors can jump to source)
6. Works with at least one client (VS Code LSP test, or manual JSON-RPC)

## Estimated effort

- ~200-300 LOC for the LSP binary (tower-lsp boilerplate + recall bridge)
- 0 LOC changes to existing code_index library (pure wrapper)
- ~2 new dependencies (tower-lsp, tokio if not already present)
- ~1-2 hours implementation for a focused worker

## Risks

- `tower-lsp` version compatibility with `lsp-types` — pin versions
- `recall_blocking` in `spawn_blocking` may have latency on first query
  if embeddings need to be computed — consider pre-warming
- Claude Code's LSP integration path is still evolving — may need MCP
  bridge instead of direct LSP
