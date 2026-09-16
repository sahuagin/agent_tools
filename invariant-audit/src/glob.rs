//! Root-relative path globs with the Python audit's semantics: `*` and `?`
//! never cross `/`, `**` spans directories, `[seq]` / `[!seq]` are character
//! classes. `Cargo.toml` is the root manifest, `**/Cargo.toml` every manifest;
//! there is no basename fallback, so a slashless exclude cannot silently widen
//! to the whole tree.

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobMatcher};

pub fn compile(pattern: &str) -> Result<GlobMatcher> {
    if pattern.contains('/')
        && pattern.contains("[!")
        && pattern[pattern.find("[!").unwrap()..].contains("/]")
    {
        anyhow::bail!("glob {pattern:?}: '/' inside a character class");
    }
    let g = GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .with_context(|| format!("glob {pattern:?}: cannot translate"))?;
    Ok(g.compile_matcher())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, rel: &str) -> bool {
        compile(p).unwrap().is_match(rel)
    }

    #[test]
    fn star_does_not_cross_slash_but_doublestar_does() {
        assert!(m("src/*.rs", "src/a.rs"));
        assert!(!m("src/*.rs", "src/x/a.rs"));
        assert!(m("src/**/*.rs", "src/x/y/a.rs"));
        assert!(m("src/**/*.rs", "src/a.rs"));
        assert!(m("**/Cargo.toml", "Cargo.toml"));
        assert!(m("**/Cargo.toml", "a/b/Cargo.toml"));
    }

    #[test]
    fn root_relative_no_basename_fallback() {
        assert!(m("Cargo.toml", "Cargo.toml"));
        assert!(!m("Cargo.toml", "crates/x/Cargo.toml"));
        assert!(m("target/**", "target/debug/x"));
        assert!(!m("target/**", "crates/target/x"));
    }

    #[test]
    fn character_classes_and_question() {
        assert!(m("src/[ab].rs", "src/a.rs"));
        assert!(!m("src/[!ab].rs", "src/a.rs"));
        assert!(m("src/?.rs", "src/a.rs"));
        assert!(!m("src/?.rs", "src/ab.rs"));
    }

    #[test]
    fn unterminated_class_is_an_error() {
        assert!(compile("src/[ab.rs").is_err());
    }
}
