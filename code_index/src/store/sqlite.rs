//! Sqlite-backed `Store` implementation.
//!
//! Vector recall: brute-force cosine over all embeddings for the requested
//! model, top-K via `BinaryHeap`. Adequate up to ~tens of thousands of
//! chunks; if/when scale demands, swap in `sqlite-vec` (feature-gate +
//! conditional ANN path) without changing the trait surface.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use super::schema::SCHEMA_V1;
use crate::{Chunk, ChunkId, ChunkKind, Edge, EdgeKind, Store};

pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    /// Open or create the index DB at the default path
    /// (`$CODE_INDEX_DB` env var, else `$HOME/.local/share/code_index/index.db`).
    pub fn open_default() -> Result<Self> {
        Self::open_at(&default_db_path())
    }

    /// Open or create the index DB at an explicit path.
    pub fn open_at(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory store. Intended for tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
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
                [now_secs()],
            )?;
            conn.execute_batch("COMMIT")?;
        }
        Ok(())
    }
}

pub fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("CODE_INDEX_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".local/share/code_index/index.db")
}

// ──────────────────────────────────────────────────────────────────────────
// f32 vector serialization (LE bytes BLOB) — local copy to avoid a cross-
// crate dep on `agent::embed` for the scaffold. Dedup later if/when we
// extract a shared embedding crate.
// ──────────────────────────────────────────────────────────────────────────

fn f32_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn blob_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na.sqrt()) * (nb.sqrt());
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ──────────────────────────────────────────────────────────────────────────
// ChunkKind / EdgeKind ↔ TEXT
// ──────────────────────────────────────────────────────────────────────────

fn chunk_kind_to_str(k: ChunkKind) -> &'static str {
    match k {
        ChunkKind::Function => "function",
        ChunkKind::Method => "method",
        ChunkKind::Class => "class",
        ChunkKind::Struct => "struct",
        ChunkKind::Enum => "enum",
        ChunkKind::Trait => "trait",
        ChunkKind::Impl => "impl",
        ChunkKind::Interface => "interface",
        ChunkKind::Type => "type",
        ChunkKind::Module => "module",
        ChunkKind::Constant => "constant",
        ChunkKind::Macro => "macro",
        ChunkKind::Test => "test",
        ChunkKind::Other => "other",
    }
}

fn chunk_kind_from_str(s: &str) -> ChunkKind {
    match s {
        "function" => ChunkKind::Function,
        "method" => ChunkKind::Method,
        "class" => ChunkKind::Class,
        "struct" => ChunkKind::Struct,
        "enum" => ChunkKind::Enum,
        "trait" => ChunkKind::Trait,
        "impl" => ChunkKind::Impl,
        "interface" => ChunkKind::Interface,
        "type" => ChunkKind::Type,
        "module" => ChunkKind::Module,
        "constant" => ChunkKind::Constant,
        "macro" => ChunkKind::Macro,
        "test" => ChunkKind::Test,
        _ => ChunkKind::Other,
    }
}

fn edge_kind_to_str(k: EdgeKind) -> &'static str {
    match k {
        EdgeKind::Calls => "calls",
        EdgeKind::References => "references",
        EdgeKind::Implements => "implements",
        EdgeKind::DefinedIn => "defined_in",
        EdgeKind::ImportedBy => "imported_by",
        EdgeKind::TestOf => "test_of",
    }
}

fn edge_kind_from_str(s: &str) -> Option<EdgeKind> {
    Some(match s {
        "calls" => EdgeKind::Calls,
        "references" => EdgeKind::References,
        "implements" => EdgeKind::Implements,
        "defined_in" => EdgeKind::DefinedIn,
        "imported_by" => EdgeKind::ImportedBy,
        "test_of" => EdgeKind::TestOf,
        _ => return None,
    })
}

// ──────────────────────────────────────────────────────────────────────────
// Store impl
// ──────────────────────────────────────────────────────────────────────────

impl Store for SqliteStore {
    fn upsert_chunk(&mut self, c: &Chunk) -> Result<ChunkId> {
        let kind = chunk_kind_to_str(c.kind);
        let file = c.file.to_string_lossy().to_string();
        let sig: i64 = c.signature_hash as i64;
        let now = now_secs();

        if c.id.0 > 0 {
            self.conn.execute(
                "UPDATE chunks
                 SET file=?, line_start=?, line_end=?, kind=?, name=?,
                     signature_hash=?, text=?, indexed_at=?
                 WHERE id=?",
                params![
                    file,
                    c.lines.start as i64,
                    c.lines.end as i64,
                    kind,
                    c.name,
                    sig,
                    c.text,
                    now,
                    c.id.0,
                ],
            )?;
            Ok(c.id)
        } else {
            self.conn.execute(
                "INSERT INTO chunks
                   (file, line_start, line_end, kind, name, signature_hash,
                    text, indexed_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    file,
                    c.lines.start as i64,
                    c.lines.end as i64,
                    kind,
                    c.name,
                    sig,
                    c.text,
                    now,
                ],
            )?;
            Ok(ChunkId(self.conn.last_insert_rowid()))
        }
    }

    fn delete_chunk(&mut self, id: ChunkId) -> Result<()> {
        self.conn
            .execute("DELETE FROM chunks WHERE id = ?", params![id.0])?;
        Ok(())
    }

    fn get_chunk(&self, id: ChunkId) -> Result<Option<Chunk>> {
        let mut stmt = self.conn.prepare(
            "SELECT file, line_start, line_end, kind, name, signature_hash, text
             FROM chunks WHERE id = ?",
        )?;
        let mut rows = stmt.query(params![id.0])?;
        if let Some(row) = rows.next()? {
            let file: String = row.get(0)?;
            let line_start: i64 = row.get(1)?;
            let line_end: i64 = row.get(2)?;
            let kind: String = row.get(3)?;
            let name: String = row.get(4)?;
            let sig: i64 = row.get(5)?;
            let text: String = row.get(6)?;
            return Ok(Some(Chunk {
                id,
                file: file.into(),
                lines: (line_start as usize)..(line_end as usize),
                kind: chunk_kind_from_str(&kind),
                name,
                signature_hash: sig as u64,
                text,
            }));
        }
        Ok(None)
    }

    fn list_chunks_by_file(&self, file: &Path) -> Result<Vec<Chunk>> {
        let f = file.to_string_lossy().to_string();
        let mut stmt = self.conn.prepare(
            "SELECT id, line_start, line_end, kind, name, signature_hash, text
             FROM chunks WHERE file = ? ORDER BY line_start",
        )?;
        let rows = stmt.query_map(params![f], |row| {
            let id: i64 = row.get(0)?;
            let line_start: i64 = row.get(1)?;
            let line_end: i64 = row.get(2)?;
            let kind: String = row.get(3)?;
            let name: String = row.get(4)?;
            let sig: i64 = row.get(5)?;
            let text: String = row.get(6)?;
            Ok(Chunk {
                id: ChunkId(id),
                file: PathBuf::from(&f),
                lines: (line_start as usize)..(line_end as usize),
                kind: chunk_kind_from_str(&kind),
                name,
                signature_hash: sig as u64,
                text,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    fn upsert_edge(&mut self, e: &Edge) -> Result<()> {
        self.conn.execute(
            "INSERT INTO edges (from_id, to_id, kind, confidence)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(from_id, to_id, kind) DO UPDATE
             SET confidence = excluded.confidence",
            params![e.from.0, e.to.0, edge_kind_to_str(e.kind), e.confidence],
        )?;
        Ok(())
    }

    fn delete_edges_from(&mut self, from: ChunkId) -> Result<()> {
        self.conn
            .execute("DELETE FROM edges WHERE from_id = ?", params![from.0])?;
        Ok(())
    }

    fn iter_edges(&self) -> Result<Vec<Edge>> {
        let mut stmt = self
            .conn
            .prepare("SELECT from_id, to_id, kind, confidence FROM edges")?;
        let rows = stmt.query_map([], |row| {
            let from: i64 = row.get(0)?;
            let to: i64 = row.get(1)?;
            let kind: String = row.get(2)?;
            let conf: f64 = row.get(3)?;
            Ok((from, to, kind, conf))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (from, to, kind, conf) = r?;
            if let Some(k) = edge_kind_from_str(&kind) {
                out.push(Edge {
                    from: ChunkId(from),
                    to: ChunkId(to),
                    kind: k,
                    confidence: conf as f32,
                });
            }
            // Unknown edge kinds are dropped silently — they may exist on
            // schema versions we don't recognize. Better to ignore than
            // panic; ingestion path will rewrite them on next pass.
        }
        Ok(out)
    }

    fn upsert_embedding(
        &mut self,
        id: ChunkId,
        model: &str,
        vec: &[f32],
    ) -> Result<()> {
        let blob = f32_to_blob(vec);
        self.conn.execute(
            "INSERT INTO chunk_embeddings (chunk_id, model, dims, vector, embedded_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(chunk_id, model) DO UPDATE SET
               dims = excluded.dims,
               vector = excluded.vector,
               embedded_at = excluded.embedded_at",
            params![id.0, model, vec.len() as i64, blob, now_secs()],
        )?;
        Ok(())
    }

    fn recall_top_k(
        &self,
        model: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(ChunkId, f32)>> {
        // Brute-force cosine over all embeddings for `model`.
        // Top-K via min-heap of size k (keeps the largest cosines).
        // Returns (id, score) — no chunk materialization. Caller decides
        // who deserves a heap walk via `get_chunk`.
        let mut stmt = self
            .conn
            .prepare("SELECT chunk_id, vector FROM chunk_embeddings WHERE model = ?")?;
        let rows = stmt.query_map(params![model], |row| {
            let id: i64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((id, blob))
        })?;

        // Min-heap on score so we can prune the smallest when full.
        let mut heap: BinaryHeap<Reverse<(OrderedF32, i64)>> = BinaryHeap::new();
        for r in rows {
            let (id, blob) = r?;
            let v = blob_to_f32(&blob);
            let score = cosine(query, &v);
            if heap.len() < k {
                heap.push(Reverse((OrderedF32(score), id)));
            } else if let Some(Reverse((min_score, _))) = heap.peek() {
                if score > min_score.0 {
                    heap.pop();
                    heap.push(Reverse((OrderedF32(score), id)));
                }
            }
        }
        // Drain heap into a sorted Vec (largest-first).
        let mut out: Vec<(ChunkId, f32)> = heap
            .into_iter()
            .map(|Reverse((s, id))| (ChunkId(id), s.0))
            .collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(out)
    }

    fn file_signature(&self, file: &Path) -> Result<Option<u64>> {
        let f = file.to_string_lossy().to_string();
        let result: Result<i64, _> = self.conn.query_row(
            "SELECT signature FROM file_manifest WHERE file = ?",
            params![f],
            |r| r.get(0),
        );
        match result {
            Ok(sig) => Ok(Some(sig as u64)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set_file_signature(&mut self, file: &Path, hash: u64) -> Result<()> {
        let f = file.to_string_lossy().to_string();
        self.conn.execute(
            "INSERT INTO file_manifest (file, signature, seen_at)
             VALUES (?, ?, ?)
             ON CONFLICT(file) DO UPDATE SET
               signature = excluded.signature,
               seen_at = excluded.seen_at",
            params![f, hash as i64, now_secs()],
        )?;
        Ok(())
    }
}

/// Total-ordering wrapper for f32 used inside the recall heap.
/// NaN compares as equal-and-smallest; we never expect NaN from cosine of
/// nonzero vectors, but defending against it is cheaper than tracking down
/// a panic later.
#[derive(Debug, Clone, Copy)]
struct OrderedF32(f32);

impl PartialEq for OrderedF32 {
    fn eq(&self, other: &Self) -> bool {
        self.0
            .partial_cmp(&other.0)
            .map(|o| o == std::cmp::Ordering::Equal)
            .unwrap_or(false)
    }
}
impl Eq for OrderedF32 {}
impl PartialOrd for OrderedF32 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedF32 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .partial_cmp(&other.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_chunk(name: &str, file: &str, kind: ChunkKind) -> Chunk {
        Chunk {
            id: ChunkId(0),
            file: PathBuf::from(file),
            lines: 1..10,
            kind,
            name: name.to_string(),
            signature_hash: 12345,
            text: format!("// chunk for {name}"),
        }
    }

    #[test]
    fn open_and_migrate_empty_db_is_idempotent() {
        let mut s = SqliteStore::open_in_memory().expect("open");
        // Re-running migrate on an already-migrated DB should be a no-op.
        SqliteStore::migrate(&s.conn).expect("migrate twice");
        // Empty DB: no chunks, no edges.
        assert_eq!(
            s.list_chunks_by_file(Path::new("nonexistent.rs"))
                .expect("list")
                .len(),
            0
        );
        assert_eq!(s.iter_edges().expect("edges").len(), 0);
    }

    #[test]
    fn upsert_get_and_list_round_trip() {
        let mut s = SqliteStore::open_in_memory().expect("open");

        let c1 = tmp_chunk("foo", "src/lib.rs", ChunkKind::Function);
        let c2 = tmp_chunk("Bar", "src/lib.rs", ChunkKind::Struct);
        let id1 = s.upsert_chunk(&c1).expect("insert c1");
        let id2 = s.upsert_chunk(&c2).expect("insert c2");
        assert_ne!(id1, id2);

        let got = s.get_chunk(id1).expect("get").expect("present");
        assert_eq!(got.name, "foo");
        assert_eq!(got.kind, ChunkKind::Function);
        assert_eq!(got.lines, 1..10);

        let listed = s
            .list_chunks_by_file(Path::new("src/lib.rs"))
            .expect("list");
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn delete_chunk_cascades_to_edges_and_embeddings() {
        let mut s = SqliteStore::open_in_memory().expect("open");

        let id_a = s
            .upsert_chunk(&tmp_chunk("a", "x.rs", ChunkKind::Function))
            .unwrap();
        let id_b = s
            .upsert_chunk(&tmp_chunk("b", "x.rs", ChunkKind::Function))
            .unwrap();

        s.upsert_edge(&Edge {
            from: id_a,
            to: id_b,
            kind: EdgeKind::Calls,
            confidence: 1.0,
        })
        .unwrap();
        s.upsert_embedding(id_a, "test-model", &[0.1, 0.2, 0.3])
            .unwrap();

        assert_eq!(s.iter_edges().unwrap().len(), 1);

        s.delete_chunk(id_a).unwrap();

        // ON DELETE CASCADE removes the edge and the embedding.
        assert_eq!(s.iter_edges().unwrap().len(), 0);
        let no_recall = s
            .recall_top_k("test-model", &[0.1, 0.2, 0.3], 5)
            .unwrap();
        assert!(no_recall.is_empty());
    }

    #[test]
    fn recall_top_k_orders_by_cosine_descending() {
        let mut s = SqliteStore::open_in_memory().expect("open");
        let id1 = s
            .upsert_chunk(&tmp_chunk("near", "f.rs", ChunkKind::Function))
            .unwrap();
        let id2 = s
            .upsert_chunk(&tmp_chunk("far", "f.rs", ChunkKind::Function))
            .unwrap();

        let model = "test";
        // Query: [1, 0]
        // near: [1, 0.1] (cosine ~ 0.995)
        // far:  [0, 1]   (cosine = 0.0)
        s.upsert_embedding(id1, model, &[1.0, 0.1]).unwrap();
        s.upsert_embedding(id2, model, &[0.0, 1.0]).unwrap();

        let hits = s.recall_top_k(model, &[1.0, 0.0], 5).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, id1, "near chunk should rank first");
        assert!(hits[0].1 > hits[1].1, "scores must descend");
    }

    #[test]
    fn recall_top_k_respects_k() {
        let mut s = SqliteStore::open_in_memory().expect("open");
        for i in 0..5 {
            let id = s
                .upsert_chunk(&tmp_chunk(
                    &format!("c{i}"),
                    "f.rs",
                    ChunkKind::Function,
                ))
                .unwrap();
            // Use a vector that varies slightly per i so cosines differ.
            s.upsert_embedding(id, "m", &[1.0, i as f32 * 0.1]).unwrap();
        }
        let hits = s.recall_top_k("m", &[1.0, 0.0], 3).unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn file_signature_round_trip() {
        let mut s = SqliteStore::open_in_memory().expect("open");
        let p = Path::new("src/foo.rs");

        assert!(s.file_signature(p).unwrap().is_none());

        s.set_file_signature(p, 42).unwrap();
        assert_eq!(s.file_signature(p).unwrap(), Some(42));

        // Upsert behavior — second call replaces, not appends.
        s.set_file_signature(p, 99).unwrap();
        assert_eq!(s.file_signature(p).unwrap(), Some(99));
    }

    #[test]
    fn upsert_chunk_with_existing_id_updates_in_place() {
        let mut s = SqliteStore::open_in_memory().expect("open");
        let mut c = tmp_chunk("orig", "f.rs", ChunkKind::Function);
        let id = s.upsert_chunk(&c).unwrap();

        // Mutate and re-upsert with the same id.
        c.id = id;
        c.name = "renamed".to_string();
        s.upsert_chunk(&c).unwrap();

        let listed = s.list_chunks_by_file(Path::new("f.rs")).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "renamed");
    }
}
