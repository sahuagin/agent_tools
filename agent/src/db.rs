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
    let conn = Connection::open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
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
