//! Recursive path walker that yields supported source files.
//!
//! Honors VCS ignore files by default — `.gitignore`, `.ignore`,
//! `.git/info/exclude`, and the global `core.excludesfile` — via the
//! `ignore` crate (the same engine ripgrep uses). This means:
//!
//! - Build outputs declared in a project's `.gitignore` (`target/`,
//!   `dist/`, `build/`, `.tox/`, etc.) are skipped without us hard-coding
//!   their names.
//! - User-declared exclusions in `.gitignore` win — the project author
//!   already decided what shouldn't be tracked.
//! - Dotfiles/dot-directories (`.git/`, `.jj/`, `.venv/`, etc.) are
//!   skipped via `hidden(true)`.
//!
//! A small extra `EXTRA_SKIP_DIRS` list catches well-known cache/build
//! directories for codebases that lack a `.gitignore` (e.g. ad-hoc
//! source trees outside any VCS). The set is intentionally tiny — the
//! VCS-ignore path is the load-bearing layer.

use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;

use crate::chunker::SupportedLanguage;

/// Directories we skip even if the project doesn't have a `.gitignore`.
/// Conservative: these are dirs whose contents are ~never source we want
/// indexed regardless of VCS-ignore configuration.
const EXTRA_SKIP_DIRS: &[&str] = &[
    "target",       // cargo
    "node_modules", // npm
    "__pycache__",  // python bytecode
    ".tox",
    ".pytest_cache",
    ".mypy_cache",
];

/// Walk `root` and return every file whose extension maps to a
/// `SupportedLanguage`. Honors `.gitignore` etc. when `respect_gitignore`
/// is true (the default).
///
/// The result is order-stable across runs (sorted before return) so
/// downstream artifacts like file_manifest entries land deterministically.
pub fn walk_sources(root: &Path) -> Result<Vec<PathBuf>> {
    walk_sources_with(root, true)
}

/// Like [`walk_sources`], but lets the caller disable VCS-ignore
/// honoring — e.g. for the `--no-gitignore` CLI flag.
pub fn walk_sources_with(root: &Path, respect_gitignore: bool) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();

    // ignore::WalkBuilder treats a single-file argument as "walk this
    // file" — no traversal needed but the builder API still works.
    let mut wb = WalkBuilder::new(root);
    wb.standard_filters(false) // we control which filters apply explicitly
        .hidden(true)
        .git_ignore(respect_gitignore)
        .git_exclude(respect_gitignore)
        .git_global(respect_gitignore)
        .ignore(true) // honor `.ignore` files regardless of VCS setting
        .require_git(false) // honor .gitignore even outside a git repo
        .filter_entry(|entry| {
            // Cheap extra cull for well-known build/cache dirs that some
            // codebases don't list in .gitignore.
            if let Some(name) = entry.file_name().to_str() {
                if entry.file_type().is_some_and(|t| t.is_dir()) && EXTRA_SKIP_DIRS.contains(&name)
                {
                    return false;
                }
            }
            true
        });

    for result in wb.build() {
        // Surface the entry, but skip walk errors (permission denied on
        // a single file shouldn't abort the whole ingest). Real config
        // problems would surface as the WalkBuilder failing earlier.
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if SupportedLanguage::from_path(p).is_some() {
            out.push(p.to_path_buf());
        }
    }
    out.sort();
    Ok(out)
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

    fn write(p: &Path, content: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

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

    #[test]
    fn walks_supported_files_and_skips_extra_dirs_without_gitignore() {
        let tmp = tempdir();
        touch(&tmp.join("a.rs"));
        touch(&tmp.join("sub/b.py"));
        touch(&tmp.join("readme.md")); // unsupported ext
        touch(&tmp.join("target/cached.rs")); // EXTRA_SKIP_DIRS
        touch(&tmp.join(".git/HEAD")); // hidden(true) skips
        touch(&tmp.join("node_modules/leftpad/index.rs"));
        touch(&tmp.join("__pycache__/foo.py"));

        let mut found = walk_sources(&tmp).unwrap();
        found.sort();

        let names: Vec<_> = found
            .iter()
            .map(|p| p.strip_prefix(&tmp).unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(names.contains(&"a.rs".to_string()), "got: {names:?}");
        assert!(names.contains(&"sub/b.py".to_string()), "got: {names:?}");
        assert!(!names.iter().any(|n| n.contains("target")));
        assert!(!names.iter().any(|n| n.contains(".git")));
        assert!(!names.iter().any(|n| n.contains("node_modules")));
        assert!(!names.iter().any(|n| n.contains("__pycache__")));
        assert!(!names.iter().any(|n| n.ends_with(".md")));
    }

    #[test]
    fn gitignore_excludes_user_declared_dirs() {
        let tmp = tempdir();
        // Establish a fake git root so .gitignore is honored.
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        write(&tmp.join(".gitignore"), "vendored/\nbuild_output/\n");
        touch(&tmp.join("src/lib.rs"));
        touch(&tmp.join("vendored/some_dep.py"));
        touch(&tmp.join("build_output/cached.rs"));

        let found = walk_sources(&tmp).unwrap();
        let names: Vec<_> = found
            .iter()
            .map(|p| p.strip_prefix(&tmp).unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(names.contains(&"src/lib.rs".to_string()));
        assert!(!names.iter().any(|n| n.contains("vendored")));
        assert!(!names.iter().any(|n| n.contains("build_output")));
    }

    #[test]
    fn no_gitignore_flag_includes_ignored_paths() {
        let tmp = tempdir();
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        write(&tmp.join(".gitignore"), "should_be_ignored/\n");
        touch(&tmp.join("src/lib.rs"));
        touch(&tmp.join("should_be_ignored/leak.rs"));

        let with_ignore = walk_sources_with(&tmp, true).unwrap();
        let without_ignore = walk_sources_with(&tmp, false).unwrap();

        assert!(!with_ignore
            .iter()
            .any(|p| p.to_string_lossy().contains("should_be_ignored")));
        assert!(without_ignore
            .iter()
            .any(|p| p.to_string_lossy().contains("should_be_ignored")));
    }

    #[test]
    fn gitignore_honored_outside_git_repo() {
        // require_git(false) should make .gitignore work even without
        // a .git/ directory present — useful when ingesting a source tree
        // that's not VCS-tracked but has authored .gitignore patterns.
        let tmp = tempdir();
        write(&tmp.join(".gitignore"), "skip_me/\n");
        touch(&tmp.join("src/lib.rs"));
        touch(&tmp.join("skip_me/x.py"));

        let found = walk_sources(&tmp).unwrap();
        assert!(found
            .iter()
            .any(|p| p.to_string_lossy().ends_with("src/lib.rs")));
        assert!(!found
            .iter()
            .any(|p| p.to_string_lossy().contains("skip_me")));
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
}
