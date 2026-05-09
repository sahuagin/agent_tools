//! Recursive path walker that yields supported source files.
//!
//! Skips a small set of always-uninteresting directories (vcs metadata,
//! build outputs, language-tool caches). The list is conservative — easy
//! to extend if real workloads turn up false negatives, but kept short so
//! we don't accidentally hide legitimate user code.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::chunker::SupportedLanguage;

/// Directories we never descend into during ingest. Match by basename.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".jj",
    ".hg",
    ".svn",
    "target",        // cargo
    "node_modules",  // npm
    ".venv",         // python
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    "dist",
    "build",
    ".idea",
    ".vscode",
];

/// Walk `root` and return every file whose extension maps to a
/// `SupportedLanguage`. The result is order-stable across runs because
/// we sort each directory's entries by name before recursing.
pub fn walk_sources(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_inner(root, &mut out)?;
    Ok(out)
}

fn walk_inner(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        if SupportedLanguage::from_path(path).is_some() {
            out.push(path.to_path_buf());
        }
        return Ok(());
    }

    let mut entries: Vec<PathBuf> = std::fs::read_dir(path)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();

    for p in entries {
        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if SKIP_DIRS.contains(&name) {
                continue;
            }
        }
        if p.is_dir() {
            walk_inner(&p, out)?;
        } else if p.is_file() {
            if SupportedLanguage::from_path(&p).is_some() {
                out.push(p);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, b"").unwrap();
    }

    #[test]
    fn walks_supported_files_and_skips_uninteresting_dirs() {
        let tmp = tempdir();
        touch(&tmp.join("a.rs"));
        touch(&tmp.join("sub/b.py"));
        touch(&tmp.join("readme.md")); // unsupported ext
        touch(&tmp.join("target/cached.rs")); // skipped dir
        touch(&tmp.join(".git/HEAD")); // skipped dir
        touch(&tmp.join("node_modules/leftpad/index.rs")); // skipped dir

        let mut found = walk_sources(&tmp).unwrap();
        found.sort();

        let names: Vec<_> = found
            .iter()
            .map(|p| p.strip_prefix(&tmp).unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(names.contains(&"a.rs".to_string()), "got: {names:?}");
        assert!(
            names.contains(&"sub/b.py".to_string()),
            "got: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("target")),
            "target should be skipped"
        );
        assert!(
            !names.iter().any(|n| n.contains(".git")),
            ".git should be skipped"
        );
        assert!(
            !names.iter().any(|n| n.contains("node_modules")),
            "node_modules should be skipped"
        );
        assert!(
            !names.iter().any(|n| n.ends_with(".md")),
            "unsupported extensions should be skipped"
        );
    }

    #[test]
    fn walks_a_single_file_argument() {
        let tmp = tempdir();
        let f = tmp.join("solo.rs");
        touch(&f);
        let found = walk_sources(&f).unwrap();
        assert_eq!(found, vec![f]);
    }

    #[test]
    fn order_is_stable() {
        let tmp = tempdir();
        for n in ["c.rs", "a.rs", "b.py"] {
            touch(&tmp.join(n));
        }
        let r1 = walk_sources(&tmp).unwrap();
        let r2 = walk_sources(&tmp).unwrap();
        assert_eq!(r1, r2, "two walks of the same tree must yield same order");
    }

    /// Cheap tempdir helper. Avoids a dep on `tempfile` for one trivial
    /// usage; cleanup happens at process exit (fine for unit tests).
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let unique = format!(
            "code_index_walker_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let dir = base.join(unique);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
