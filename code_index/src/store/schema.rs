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
