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
install -m 0755 reindex-if-changed reindex-after-push ~/.local/bin/
sudo sysrc code_index_mcp_enable="YES"
sudo sysrc code_index_mcp_listen="10.1.1.172:7622"   # match the MCP client config URL
sudo sysrc code_index_mcp_db="/home/tcovert/.cache/code_index/mu.db"  # serve from the cache family
sudo service code_index_mcp start
```

Reindex cron (one line per repo; `reindex-if-changed` no-ops unless the repo's
main moved since the last successful ingest):

```crontab
*/15 * * * * /home/tcovert/.local/bin/reindex-if-changed /home/tcovert/src/public_github/mu >/dev/null 2>&1
```

`reindex-after-push` is the manual one-shot form of the same ingest (first
index of a new repo, or force-refresh outside the cron cadence).

`rc.conf` knobs (all `code_index_mcp_`-prefixed): `enable`, `listen`, `user`
(default `tcovert` — a script default is sufficient; rc.subr picks it up),
`db` (default DB when a `code_recall` call passes no `db` arg), `serve`
(wrapper path), `logfile` (`/var/log/code_index_mcp.log`).

## What is served: `[[code_index.sources]]`

One config section in `~/.config/agent/config.toml` is the single source of
truth for what is indexed. It drives the reindex cron, the `db` names this
service accepts, and the `code_sources` listing — so those three can't drift
apart:

```toml
[code_index]
# cache_dir = "~/.cache/code_index"   # default  (~ and $HOME are resolved)

[[code_index.sources]]
path = "~/src/public_github/mu"

[[code_index.sources]]
path = "~/src/public_github/agent_tools"
```

Then cron needs exactly one line for every repo — adding a repo is a config
edit, not a crontab edit:

```
*/15 * * * * $HOME/.local/bin/reindex-if-changed >/dev/null 2>&1
```

`reindex-if-changed` gets the list from `code-index sources --porcelain`
rather than parsing the TOML itself, so the cron and the service always agree.
An explicit path argument (`reindex-if-changed /path/to/repo`) still works for
manual runs.

## Operational notes

- **DB selection is per-call.** `code_recall` / `code_status` take a `db`
  argument: a configured source name, an absolute path, or a bare name
  resolving to `<cache_dir>/<name>.db`. `CODE_INDEX_DB` is only the no-arg
  default. Call **`code_sources`** to see the valid names and how fresh each
  index is — that is the discovery verb; nothing should be guessing repo names.
- **One index family (2026-07-22): the cache.** Ingest (cron + manual) writes
  `<cache_dir>/<name>.db` and the service serves from it (the
  `code_index_mcp_db` rc knob). Per-repo `.code_index/` dirs are RETIRED — if
  one exists, `reindex-if-changed` still prefers it (legacy branch), so don't
  recreate them. The cache is rebuildable by definition: deleting a db and
  re-running ingest is the supported recovery path. What the family contains is
  now declared by `[[code_index.sources]]` (above) rather than inferred; names
  key by the source's `name` or its path basename, and origin-URL keys are
  at-lcn.
- **An index in the cache dir but not in the config still resolves.**
  Configuring sources adds management (cron freshness, listing); it never
  takes away a db that already worked. `code_sources` reports those separately
  as unmanaged.
- **The read paths never create a database** (at-jjw, done). `code_recall`,
  `code_status`, and `code-index status` open existing files only; a missing db
  is a typed error naming the resolved path and listing what IS available.
  Previously sqlite's default open semantics created an empty db on the *query*
  path, and that empty file then shadowed the real error forever — a typo'd
  name became a permanent "No results found. Has the repository been indexed?".
  Only `ingest` / `init` create.
- **Tilde paths are expanded before they touch the filesystem.** A `db`
  argument or `--db` flag written `~/.cache/code_index/mu.db` used to be taken
  as a bare NAME and joined under the cache dir, producing
  `<cache_dir>/~/.cache/code_index/mu.db.db` — and create-on-open then made
  that tree. That is where the literal `~` directory in the cache came from.
- Beware: a repo with a per-repo `.code_index/index.db` (what the reindex cron
  refreshes when that dir exists) is NOT what its bare cache name resolves to
  — pass the absolute per-repo path for fresh data.
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
