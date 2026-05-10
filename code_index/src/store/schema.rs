//! SQL schema for the sqlite-backed `Store`.
//!
//! Versioned via `schema_version`; migrate by appending `SCHEMA_VN` strings
//! and bumping the if-block in `migrate()`.

pub(super) const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS chunks (
    id              INTEGER PRIMARY KEY,
    file            TEXT    NOT NULL,
    line_start      INTEGER NOT NULL,
    line_end        INTEGER NOT NULL,
    kind            TEXT    NOT NULL,
    name            TEXT    NOT NULL,
    signature_hash  INTEGER NOT NULL,
    text            TEXT    NOT NULL,
    indexed_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_chunks_file ON chunks(file);
CREATE INDEX IF NOT EXISTS ix_chunks_name ON chunks(name);

CREATE TABLE IF NOT EXISTS edges (
    from_id     INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    to_id       INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    kind        TEXT    NOT NULL,
    confidence  REAL    NOT NULL,
    PRIMARY KEY (from_id, to_id, kind)
);
CREATE INDEX IF NOT EXISTS ix_edges_from ON edges(from_id);
CREATE INDEX IF NOT EXISTS ix_edges_to   ON edges(to_id);

CREATE TABLE IF NOT EXISTS chunk_embeddings (
    chunk_id    INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    model       TEXT    NOT NULL,
    dims        INTEGER NOT NULL,
    vector      BLOB    NOT NULL,
    embedded_at INTEGER NOT NULL,
    PRIMARY KEY (chunk_id, model)
);
CREATE INDEX IF NOT EXISTS ix_chunk_embeddings_model ON chunk_embeddings(model);

CREATE TABLE IF NOT EXISTS file_manifest (
    file       TEXT    PRIMARY KEY,
    signature  INTEGER NOT NULL,
    seen_at    INTEGER NOT NULL
);
"#;

/// V2 adds the FTS5 lexical index over chunks(name, text) and the
/// triggers that keep it in sync. External-content mode (content='chunks')
/// avoids duplicating name/text — FTS5 reads them from the underlying
/// table during query.
///
/// Tokenizer choice — default `unicode61`. Splits on `_` so identifiers
/// like `read_parquet_from_s3` become 4 tokens (`read`, `parquet`,
/// `from`, `s3`) and a query for `parquet` matches them all. This is
/// the right behavior for code search: users typically know one or two
/// of the constituent words, not the full snake-cased identifier. The
/// cost is reduced precision when the user DOES know the exact symbol;
/// semantic recall covers that case.
///
/// On migration, `INSERT INTO chunks_fts(chunks_fts) VALUES('rebuild')`
/// backfills the index from the existing chunks rows. Cheap relative to
/// the embedding pass.
pub(super) const SCHEMA_V2: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    name,
    text,
    content='chunks',
    content_rowid='id',
    tokenize='unicode61'
);

-- Insert: mirror new chunks into the FTS index.
CREATE TRIGGER IF NOT EXISTS chunks_fts_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, name, text)
        VALUES (new.id, new.name, new.text);
END;

-- Delete: 'delete' command shape required by external-content FTS5.
CREATE TRIGGER IF NOT EXISTS chunks_fts_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, name, text)
        VALUES ('delete', old.id, old.name, old.text);
END;

-- Update: drop old, insert new.
CREATE TRIGGER IF NOT EXISTS chunks_fts_au AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, name, text)
        VALUES ('delete', old.id, old.name, old.text);
    INSERT INTO chunks_fts(rowid, name, text)
        VALUES (new.id, new.name, new.text);
END;

-- Rebuild from any existing rows. No-op for fresh DBs; backfills
-- existing chunks for DBs that were created before V2 landed.
INSERT INTO chunks_fts(chunks_fts) VALUES ('rebuild');
"#;
