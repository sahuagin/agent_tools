//! Storage implementations for `Store`.
//!
//! Initial backend is sqlite (mirroring `agent/src/db.rs` patterns); future
//! candidates (redb, lance, duckdb) plug in here behind the same trait.

pub mod schema;
pub mod sqlite;

pub use sqlite::SqliteStore;
