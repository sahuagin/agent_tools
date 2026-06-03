# code-index-mcp deployment (FreeBSD rc.d)

Keeps `code-index-mcp --listen` persistently up as a supervised service, so MCP
clients (Claude Code, mu-serve) can call `code_recall` / `code_status` over
rmcp Streamable HTTP (axum route `/mcp`). The reindex cron keeps the DBs fresh;
this service serves them — the two are fully decoupled.

Deployed on threadripper 2026-06-03: `http://10.1.1.172:7622/mcp`.

## Files

- `code_index_mcp.rc` — rc.d script. Installed at
  `/usr/local/etc/rc.d/code_index_mcp` (root:wheel 0555). Runs the wrapper
  under `daemon(8) -r` (auto-restart) as an unprivileged user via rc.subr's
  `${name}_user` switch.
- `code-index-mcp-serve` — launch wrapper. Installed at
  `~/.local/bin/code-index-mcp-serve`. Reads `OPENROUTER_API_KEY` from
  `~/.config/agent/config.toml` via `tq` *inside* the process (never on argv,
  never in `ps`), pins the default `CODE_INDEX_DB`, then `exec`s the binary so
  daemon(8) supervises the server itself, not a shell.

## Install

```sh
sudo install -m 0555 code_index_mcp.rc /usr/local/etc/rc.d/code_index_mcp
install -m 0755 code-index-mcp-serve ~/.local/bin/code-index-mcp-serve
sudo sysrc code_index_mcp_enable="YES"
sudo sysrc code_index_mcp_listen="10.1.1.172:7622"   # match the MCP client config URL
sudo service code_index_mcp start
```

`rc.conf` knobs (all `code_index_mcp_`-prefixed): `enable`, `listen`, `user`
(default `tcovert` — a script default is sufficient; rc.subr picks it up),
`db` (default DB when a `code_recall` call passes no `db` arg), `serve`
(wrapper path), `logfile` (`/var/log/code_index_mcp.log`).

## Operational notes

- **DB selection is per-call.** `code_recall`'s `db` argument picks the
  database: an absolute path, or a bare name resolving to
  `~/.cache/code_index/<name>.db`. `CODE_INDEX_DB` is only the no-arg default.
  Beware: a repo with a per-repo `.code_index/index.db` (what the reindex cron
  refreshes) is NOT what its bare cache name resolves to — pass the absolute
  per-repo path for fresh data.
- **Do not add `daemon -u`.** rc.subr's `${name}_user` already drops privilege
  via `su -m` (`/etc/rc.subr` line ~1513); stacking `daemon -u` on top calls
  `setusercontext()` from an already-dropped process and crash-loops under
  `-r`. Same lesson as the `claude_proxy` / `c137_memory_worker` services.
- **`su -m` preserves the invoker's env** — that's why the rc script injects
  `HOME=` explicitly; without it the wrapper resolves `~` against root's env.
- **Pidfile** lives in a user-owned `/var/run/code_index_mcp/` subdir
  (created in `start_precmd`) because the daemon runs unprivileged and can't
  write `/var/run` directly.
- **Client handshake** (anything hand-rolled must replicate this; rmcp's
  `StreamableHttpClientTransport` does it for free): `initialize` returns an
  `Mcp-Session-Id` header; every subsequent POST needs that header plus
  `MCP-Protocol-Version` and `Accept: application/json, text/event-stream`,
  and responses are SSE-framed (`data: {json}`).
- **Embedding key**: the wrapper's `tq` lookup exists only because query-time
  embedding goes through OpenRouter. Switching code-index to the local ollama
  embedder (bead `at-ollama-embedder-option-6ky`) removes the service's only
  secret — requires re-embedding every DB with the local model first.
