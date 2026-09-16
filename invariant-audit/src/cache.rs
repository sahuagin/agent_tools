//! A content-hash cache of `#[cfg(test)]` regions per file, so an audit that
//! runs on every checkout does not reparse files that have not changed. Keyed
//! on the file's bytes and the grammar version; stored as JSON under the
//! repository's `target/invariant-audit/` (or `.invariant-audit-cache/` when
//! there is no target dir), and never required: a missing or unreadable cache
//! is an empty one.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const GRAMMAR_VERSION: &str = concat!("tree-sitter-rust ", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedRegion {
    pub start: usize,
    pub end: usize,
}

impl CachedRegion {
    pub fn contains(&self, byte: usize) -> bool {
        (self.start..self.end).contains(&byte)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    grammar: String,
    /// sha256 of the file bytes -> regions
    entries: HashMap<String, Vec<CachedRegion>>,
}

pub struct RegionCache {
    path: Option<PathBuf>,
    inner: Mutex<CacheFile>,
    dirty: Mutex<bool>,
}

impl RegionCache {
    pub fn open(root: &Path, enabled: bool) -> Self {
        let path = if enabled {
            let dir = if root.join("target").is_dir() {
                root.join("target").join("invariant-audit")
            } else {
                root.join(".invariant-audit-cache")
            };
            Some(dir.join("cfg-test-regions.json"))
        } else {
            None
        };
        let mut file = CacheFile::default();
        if let Some(p) = &path {
            if let Ok(text) = fs::read_to_string(p) {
                if let Ok(parsed) = serde_json::from_str::<CacheFile>(&text) {
                    if parsed.grammar == GRAMMAR_VERSION {
                        file = parsed;
                    }
                }
            }
        }
        file.grammar = GRAMMAR_VERSION.to_string();
        Self {
            path,
            inner: Mutex::new(file),
            dirty: Mutex::new(false),
        }
    }

    /// The `#[cfg(test)]` regions of a Rust file, from the cache or from a
    /// fresh parse. A file the grammar cannot parse is an error naming the
    /// first bad token: the audit will not guess where test code ends.
    pub fn regions_for(&self, rel: &str, bytes: &[u8]) -> Result<Vec<CachedRegion>> {
        let key = hex(&Sha256::digest(bytes));
        if let Some(hit) = self.inner.lock().unwrap().entries.get(&key) {
            return Ok(hit.clone());
        }
        let tree = tree_regions::parse_rust(bytes)
            .ok_or_else(|| anyhow::anyhow!("{rel}: tree-sitter could not parse the file"))?;
        let errors = tree_regions::parse_errors(&tree);
        if let Some(first) = errors.first() {
            bail!(
                "{rel}:{}:{}: the Rust grammar could not parse this file ({} error{}); the audit will not guess where #[cfg(test)] regions end — fix the file or exclude it",
                first.line,
                first.column,
                errors.len(),
                if errors.len() == 1 { "" } else { "s" }
            );
        }
        let regions: Vec<CachedRegion> = tree_regions::cfg_test_regions(bytes, &tree)
            .into_iter()
            .map(|r| CachedRegion {
                start: r.bytes.start,
                end: r.bytes.end,
            })
            .collect();
        self.inner
            .lock()
            .unwrap()
            .entries
            .insert(key, regions.clone());
        *self.dirty.lock().unwrap() = true;
        Ok(regions)
    }

    /// Write the cache back if anything was parsed this run. Best effort: a
    /// cache that cannot be written is simply not there next time.
    pub fn flush(&self) {
        let Some(p) = &self.path else { return };
        if !*self.dirty.lock().unwrap() {
            return;
        }
        let inner = self.inner.lock().unwrap();
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string(&*inner) {
            let _ = fs::write(p, text);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
