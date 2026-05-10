# code_index

Code-aware indexing and retrieval for agentic workflows. Walks a repo,
chunks each source file at definition boundaries via tree-sitter, embeds
the chunks via a configurable embedder, and stores everything in a
single sqlite file. Recall combines semantic embedding similarity (via
cosine over batched embeddings) with lexical FTS5 BM25, fused via
Reciprocal Rank Fusion. A graph layer extracts call/reference edges
during a separate pass, enabling caller-tree navigation and PageRank
centrality.

The intended use case is "give a sub-agent a focused slice of an
unfamiliar codebase" without re-reading every file each session post-
compaction. Pairs naturally with grep — semantic recall finds code
when you describe behavior in prose; grep wins when you know the
exact symbol or string.

## Install

```sh
cd ~/src/agent_tools
cargo build --release -p code_index
ln -sf "$PWD/target/release/code-index" ~/.local/bin/code-index
```

Optional: load the OpenRouter API key from `~/.config/agent/config.toml`
into env so embeddings work. Without a key, code_index falls back to a
deterministic-but-meaningless `MockEmbedder` and `recall` will be
dimensionally correct but semantically useless.

```sh
export OPENROUTER_API_KEY="$(tq -f ~/.config/agent/config.toml -r 'openrouter.api_key' 2>/dev/null)"
```

## Quick start

```sh
cd /path/to/some/repo
code-index init                   # opt into per-project DB at .code_index/index.db
code-index ingest .               # walk + chunk + embed
code-index graph build            # extract edges (re-parse pass)
code-index recall "<query>" -n 10 --full
code-index status                 # what's indexed
```

`code-index init` is optional — without it, code-index falls back to
`~/.cache/code_index/<basename-of-cwd>.db`. The init pattern is git/jj-
style: marker dir lets commands auto-discover the right DB by walking
up from cwd.

## DB resolution

In order:
1. `--db <path>` flag, if given.
2. `$CODE_INDEX_DB` env var, if non-empty.
3. Walk up from cwd looking for `.code_index/index.db` (or just the
   `.code_index/` directory; the DB is created on first use).
4. `~/.cache/code_index/<basename-of-cwd>.db`.

## Recall modes

| Mode | What it does |
|---|---|
| `hybrid` (default) | Pulls 2k from each of semantic + lexical, fuses via RRF |
| `semantic` | Embedding cosine similarity only |
| `lexical` | FTS5 BM25 only — best for "I know the keyword" queries |

Examples:
```sh
code-index recall "promote borrowed to owned" -n 5 --full           # hybrid
code-index recall "FixedBuffer" -n 5 --mode lexical                  # exact-symbol
code-index recall "where do we read parquet from S3" -n 10           # concept
```

For natural-language queries, tests over-rank because their function
names tend to be descriptive prose (e.g. `test_unknown_command_returns_error`).
If you find tests dominating, try `--mode lexical` or refine the query
with a rare/specific token (a method name, an unusual identifier).

## Graph operations

Edges are extracted by `code-index graph build`, which re-parses every
file in the manifest, captures `@reference.X` tags from tags.scm, and
resolves each by name lookup against the chunks table. Resolution is
intentionally simple at v1:

- Single same-file match → confidence 1.0
- Single any-file match → 0.85 (cross-file unambiguous)
- Multiple matches → same-file pick at 0.85, otherwise 0.6
- No match → unresolved (external / std::* / out-of-tree)

After build:

```sh
code-index graph stats                              # nodes / edges / components
code-index graph centrality -n 20                   # PageRank top-N
code-index graph communities -n 10 --min-size 10    # connected components
code-index graph path <from-id> <to-id>             # shortest path
```

Find chunk IDs with sqlite3:
```sh
sqlite3 ~/.cache/code_index/<repo>.db \
  "SELECT id, kind, name, file FROM chunks WHERE name = 'X' LIMIT 5;"
```

## Languages

Today: Rust (`.rs`), Python (`.py`, `.pyi`).

`tags.scm` queries are vendored from upstream `tree-sitter-<lang>` repos
under MIT license — see `src/chunker/queries/ATTRIBUTIONS.md`. Adding a
language is ~30 minutes: add the grammar crate dep, vendor its
`queries/tags.scm`, register the file extension in
`SupportedLanguage::from_extension`.

## Embedder configuration

Defaults to `qwen/qwen3-embedding-8b` via OpenRouter. Override:

| Env var | Effect |
|---|---|
| `OPENROUTER_API_KEY` | Required for real embeddings |
| `CODE_INDEX_EMBED_MODEL` | Override model ID |
| `CODE_INDEX_EMBED_BASE_URL` | Override the API endpoint |
| `AGENT_EMBED_MODEL` | Fallback if `CODE_INDEX_EMBED_MODEL` not set |
| `AGENT_EMBED_BASE_URL` | Fallback if `CODE_INDEX_EMBED_BASE_URL` not set |

The fallback chain mirrors `agent_tools/agent`'s embedder so a single
`config.toml` update propagates to both.

## Performance notes

- Chunks per file: typically 5-20 for Rust/Python; specific large
  modules can balloon (a 810KB `tests` module showed up in
  pi_agent_rust). The embed-input is capped at 24k chars so oversize
  chunks don't trip embedding-API token limits — full text still in DB.
- Embedding throughput: with `--embed-concurrency 8` against
  qwen3-embedding-8b, sustained ~70 chunks/sec. Higher concurrency
  triggers OpenRouter's rate limit and reduces throughput due to backoff
  waits — stay at 8 unless you have a reason.
- Recall: brute-force cosine over all embeddings for a model, top-K via
  min-heap. Adequate up to ~tens of thousands of chunks; larger
  workloads will want sqlite-vec ANN (filed as future work).
- `graph build` re-parses every file in the manifest. ~70 sec for
  pi_agent_rust's 1755 files. Re-running on an unchanged tree is the
  same cost — no incremental update yet.

## Workflow integration

Symlink the binary to PATH and the per-project pattern is:

```sh
cd ~/src/<project>
code-index init
code-index ingest .
code-index graph build
# subsequent recall/graph commands auto-find .code_index/index.db
```

Re-ingesting after code changes is cheap thanks to the file_manifest
hash check — only files whose content hash changed are re-chunked.
Embeddings for unchanged chunks persist (their content hash matches).
The embedding pass picks up only chunks lacking an embedding for the
target model, so adding new files or switching models doesn't redo work
that's already done.

## Architecture decisions

See `.beads/issues.jsonl` for the live design log; ADRs filed as
issues with `adr` label.

Key calls:
- BSD-3-Clause license; vendored tags.scm queries dual-distributed
  under their original MIT terms.
- Storage: sqlite (`rusqlite` bundled). FTS5 for lexical recall. The
  `Store` trait is the durable boundary; redb / lance / duckdb are
  candidate swap-outs documented in design.
- Two-pass embedding: ingest persists chunks; `embed_pending_concurrent`
  walks chunks lacking an embedding for the embedder's model. Cheap to
  re-run after partial failures (NOT EXISTS query).
- Graph: in-memory, hydrated from the store on demand. petgraph
  underneath; PageRank hand-rolled because petgraph 0.6's `page_rank`
  benchmarked catastrophically slow at 25k nodes.
- Concurrent embedding: `std::thread::scope` with per-worker channels,
  not async. async would be the right shape at much larger
  concurrencies; threads-with-channels is right at the 8-16 in-flight-
  HTTP-requests scale and avoids spreading async-ness through the trait.

## License

BSD-3-Clause for code_index itself; see `LICENSE` at workspace root.
Vendored tags.scm files retain their upstream licensing — see
`src/chunker/queries/ATTRIBUTIONS.md`.
