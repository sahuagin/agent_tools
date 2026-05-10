use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::PathBuf;

pub fn db_path() -> PathBuf {
    if let Ok(p) = std::env::var("AGENT_DB") {
        return PathBuf::from(p);
    }
    let mut p = dirs_home().unwrap_or_else(|| PathBuf::from("."));
    p.push(".local/share/agent.sqlite");
    p
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

pub fn open() -> Result<Connection> {
    let path = db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version  INTEGER NOT NULL,
            applied  INTEGER NOT NULL
        );",
    )?;

    let version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    if version < 1 {
        conn.execute_batch("BEGIN")?;
        conn.execute_batch(SCHEMA_V1)?;
        conn.execute(
            "INSERT INTO schema_version (version, applied) VALUES (1, ?)",
            [chrono::Utc::now().timestamp()],
        )?;
        conn.execute_batch("COMMIT")?;
    }

    if version < 2 {
        conn.execute_batch("BEGIN")?;
        conn.execute_batch(SCHEMA_V2)?;
        conn.execute(
            "INSERT INTO schema_version (version, applied) VALUES (2, ?)",
            [chrono::Utc::now().timestamp()],
        )?;
        conn.execute_batch("COMMIT")?;
    }

    if version < 3 {
        conn.execute_batch("BEGIN")?;
        conn.execute_batch(SCHEMA_V3)?;
        conn.execute(
            "INSERT INTO schema_version (version, applied) VALUES (3, ?)",
            [chrono::Utc::now().timestamp()],
        )?;
        conn.execute_batch("COMMIT")?;
    }

    if version < 4 {
        conn.execute_batch("BEGIN")?;
        conn.execute_batch(SCHEMA_V4)?;
        conn.execute(
            "INSERT INTO schema_version (version, applied) VALUES (4, ?)",
            [chrono::Utc::now().timestamp()],
        )?;
        conn.execute_batch("COMMIT")?;
    }

    // Schema ladder note: v5 (recall log) and v6 (lifecycle + event log) were
    // built on a branch that ran in production for ~2 weeks (deployed DBs reached
    // v6) but was never merged to main; v7 (scope) was added separately. Both are
    // reconciled here, so source now matches deployed DBs exactly: 1–7 in order.
    // The next migration MUST be numbered 8 or higher.
    if version < 5 {
        conn.execute_batch("BEGIN")?;
        conn.execute_batch(SCHEMA_V5)?;
        conn.execute(
            "INSERT INTO schema_version (version, applied) VALUES (5, ?)",
            [chrono::Utc::now().timestamp()],
        )?;
        conn.execute_batch("COMMIT")?;
    }

    if version < 6 {
        conn.execute_batch("BEGIN")?;
        conn.execute_batch(SCHEMA_V6)?;
        conn.execute(
            "INSERT INTO schema_version (version, applied) VALUES (6, ?)",
            [chrono::Utc::now().timestamp()],
        )?;
        conn.execute_batch("COMMIT")?;
    }

    if version < 7 {
        conn.execute_batch("BEGIN")?;
        conn.execute_batch(SCHEMA_V7)?;
        conn.execute(
            "INSERT INTO schema_version (version, applied) VALUES (7, ?)",
            [chrono::Utc::now().timestamp()],
        )?;
        conn.execute_batch("COMMIT")?;
    }

    Ok(())
}

const SCHEMA_V1: &str = "
CREATE TABLE memories (
    id          TEXT PRIMARY KEY,
    type        TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL,
    content     TEXT NOT NULL,
    source      TEXT NOT NULL DEFAULT 'curated',
    tags        TEXT NOT NULL DEFAULT '',
    cwd         TEXT NOT NULL DEFAULT '',
    is_active   INTEGER NOT NULL DEFAULT 1,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE VIRTUAL TABLE memories_fts USING fts5(
    name, description, content, tags,
    content='memories',
    content_rowid='rowid'
);

CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, name, description, content, tags)
    VALUES (new.rowid, new.name, new.description, new.content, new.tags);
END;

CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, name, description, content, tags)
    VALUES ('delete', old.rowid, old.name, old.description, old.content, old.tags);
END;

CREATE TRIGGER memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, name, description, content, tags)
    VALUES ('delete', old.rowid, old.name, old.description, old.content, old.tags);
    INSERT INTO memories_fts(rowid, name, description, content, tags)
    VALUES (new.rowid, new.name, new.description, new.content, new.tags);
END;

CREATE TABLE tasks (
    id             TEXT PRIMARY KEY,
    status         TEXT NOT NULL DEFAULT 'pending',
    objective      TEXT NOT NULL,
    task_type      TEXT NOT NULL DEFAULT 'research',
    cwd            TEXT NOT NULL DEFAULT '',
    parent_task_id TEXT REFERENCES tasks(id),
    completion_id  TEXT REFERENCES completions(id),
    result         TEXT,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

CREATE TABLE completions (
    id          TEXT PRIMARY KEY,
    task_id     TEXT REFERENCES tasks(id),
    session_id  TEXT,
    model       TEXT,
    provider    TEXT,
    task_type   TEXT,
    objective   TEXT,
    cwd         TEXT NOT NULL DEFAULT '',
    status      TEXT,
    confidence  REAL,
    tool_calls  INTEGER,
    wall_ms     INTEGER,
    created_at  INTEGER NOT NULL
);

CREATE TABLE usage_events (
    id            INTEGER PRIMARY KEY,
    completion_id TEXT NOT NULL REFERENCES completions(id),
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read    INTEGER NOT NULL DEFAULT 0,
    cache_write   INTEGER NOT NULL DEFAULT 0,
    cost_usd      REAL NOT NULL DEFAULT 0.0,
    created_at    INTEGER NOT NULL
);
";

const SCHEMA_V2: &str = "
CREATE UNIQUE INDEX IF NOT EXISTS idx_schema_version_version ON schema_version(version);
CREATE INDEX IF NOT EXISTS idx_memories_type_active_updated ON memories(type, is_active, updated_at);
CREATE INDEX IF NOT EXISTS idx_tasks_updated_at ON tasks(updated_at);
CREATE INDEX IF NOT EXISTS idx_tasks_status_updated ON tasks(status, updated_at);
CREATE INDEX IF NOT EXISTS idx_completions_task_id ON completions(task_id);
CREATE INDEX IF NOT EXISTS idx_usage_completion_id ON usage_events(completion_id);
";

const SCHEMA_V3: &str = "
CREATE TABLE memory_topic_index (
    term       TEXT NOT NULL,
    memory_id  TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    weight     REAL NOT NULL DEFAULT 1.0,
    PRIMARY KEY (term, memory_id)
);
CREATE INDEX idx_mti_term ON memory_topic_index(term);

CREATE TABLE memory_context_log (
    id          INTEGER PRIMARY KEY,
    created_at  INTEGER NOT NULL,
    cwd         TEXT NOT NULL DEFAULT '',
    signals     TEXT NOT NULL DEFAULT '',
    n_scored    INTEGER NOT NULL DEFAULT 0,
    returned    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX idx_mcl_created ON memory_context_log(created_at DESC);
";

const SCHEMA_V4: &str = "
CREATE TABLE memory_embeddings (
    memory_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    model        TEXT NOT NULL,
    dims         INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    embedded_at  INTEGER NOT NULL,
    vector       BLOB NOT NULL,
    PRIMARY KEY (memory_id, model)
);
CREATE INDEX idx_me_model ON memory_embeddings(model);
";
// v5: recall telemetry log (one row per `agent memory recall`).
const SCHEMA_V5: &str = "
CREATE TABLE memory_recall_log (
    id           INTEGER PRIMARY KEY,
    ts           INTEGER NOT NULL,
    cwd          TEXT,
    query        TEXT NOT NULL,
    k            INTEGER NOT NULL,
    type_filter  TEXT,
    top_score    REAL,
    results_json TEXT NOT NULL,
    compare_used INTEGER NOT NULL DEFAULT 0,
    fts_hits     INTEGER
);
CREATE INDEX idx_mrl_ts ON memory_recall_log(ts DESC);
";

// v6: memory lifecycle (active/archived/trashed + supersede/purge) and a
// before/after audit log. Powers archive/trash/restore/events/apply-plan.
const SCHEMA_V6: &str = "
ALTER TABLE memories ADD COLUMN lifecycle TEXT NOT NULL DEFAULT 'active';
ALTER TABLE memories ADD COLUMN archived_at INTEGER;
ALTER TABLE memories ADD COLUMN trashed_at INTEGER;
ALTER TABLE memories ADD COLUMN purge_after INTEGER;
ALTER TABLE memories ADD COLUMN superseded_by TEXT;
CREATE INDEX idx_memories_lifecycle_updated ON memories(lifecycle, updated_at);

CREATE TABLE memory_events (
    id            INTEGER PRIMARY KEY,
    ts            INTEGER NOT NULL,
    actor         TEXT NOT NULL DEFAULT 'agent',
    action        TEXT NOT NULL,
    memory_id     TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    before_json   TEXT,
    after_json    TEXT,
    reason        TEXT NOT NULL DEFAULT '',
    source_report TEXT NOT NULL DEFAULT ''
);
CREATE INDEX idx_memory_events_memory_ts ON memory_events(memory_id, ts DESC);
CREATE INDEX idx_memory_events_ts ON memory_events(ts DESC);
";

// v7: per-profile scoping. Every existing row inherits 'shared' from the
// DEFAULT (visible to both work and personal profiles); new memories are
// written with the active profile's scope. The FTS triggers (v1) don't
// reference `scope`, so this column is invisible to lexical search and fully
// backward-compatible with older binaries (which SELECT explicit columns).
const SCHEMA_V7: &str = "
ALTER TABLE memories ADD COLUMN scope TEXT NOT NULL DEFAULT 'shared';
CREATE INDEX IF NOT EXISTS idx_memories_scope ON memories(scope);
";
