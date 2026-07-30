//! Configured index sources — one config section as the single source of truth
//! for what is indexed, what the MCP service will serve, and what the reindex
//! cron maintains.
//!
//! Shape mirrors `[[kx.sources]]` (array-of-tables in
//! `~/.config/agent/config.toml`):
//!
//! ```toml
//! [code_index]
//! # cache_dir = "~/.cache/code_index"   # optional; this is the default
//!
//! [[code_index.sources]]
//! path = "~/src/public_github/mu"
//! repo = true          # default: VCS-head change detection (jj/git)
//! # name = "mu"        # optional key override (default: path basename)
//! ```
//!
//! `~/...` and `$HOME/...` are resolved at load time (see [`expand_home`]),
//! so nothing here — config, defaults, or docs — carries an absolute home
//! path that is wrong on the next machine.
//!
//! Resolution policy (`resolve`) is deliberately ADDITIVE — configuring
//! sources must never take away a db that resolves today:
//!
//! 1. absolute path -> used as given.
//! 2. configured source name -> `<cache_dir>/<name>.db`.
//! 3. unconfigured name that already exists on disk -> served anyway, so the
//!    legacy cache-family behavior is preserved.
//! 4. anything else -> a typed error naming the resolved path and listing
//!    what IS available.
//!
//! Step 4 is the at-jjw fix: a miss is an error that names itself, not a
//! silently open-created empty database that shadows the real problem forever.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Minimum size for a db file to count as a real index rather than an empty
/// shell (an open-created sqlite file is ~74k). Matches `mcp.rs`.
pub const MIN_POPULATED_DB_SIZE: u64 = 100_000;

/// One configured index source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Root directory that gets indexed.
    pub path: PathBuf,
    /// Key this source is addressed by (`db` argument, `<name>.db` in the cache).
    pub name: String,
    /// `true` (default): change detection via VCS head (jj/git).
    /// `false`: the location need not be a repository; hash its content.
    pub repo: bool,
}

impl Source {
    /// Where this source's index lives under `cache_dir`.
    pub fn db_path(&self, cache_dir: &Path) -> PathBuf {
        cache_dir.join(format!("{}.db", self.name))
    }
}

/// The parsed `[code_index]` section plus the resolved cache directory.
#[derive(Debug, Clone)]
pub struct Sources {
    entries: Vec<Source>,
    cache_dir: PathBuf,
}

/// What `resolve` could not do, phrased so the caller can say it out loud.
#[derive(Debug, Clone)]
pub struct ResolveError {
    /// The name or path the caller asked for.
    pub requested: String,
    /// Where that name resolved to on disk (absent for an unresolvable name).
    pub resolved: Option<PathBuf>,
    /// Every key the service can currently serve.
    pub available: Vec<String>,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.resolved {
            Some(p) => write!(
                f,
                "no index for {:?}: {} does not exist (or is an empty shell). \
                 Available: {}",
                self.requested,
                p.display(),
                fmt_available(&self.available),
            ),
            None => write!(
                f,
                "no index for {:?}. Available: {}",
                self.requested,
                fmt_available(&self.available),
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

fn fmt_available(available: &[String]) -> String {
    if available.is_empty() {
        "(none — nothing configured in [[code_index.sources]] and no populated \
         db in the cache directory)"
            .to_string()
    } else {
        available.join(", ")
    }
}

// ─── Deserialization ────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct RootConfig {
    #[serde(default)]
    code_index: CodeIndexSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CodeIndexSection {
    /// Where the index family lives. Default `$HOME/.cache/code_index`.
    cache_dir: Option<String>,
    sources: Vec<SourceEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct SourceEntry {
    path: String,
    name: Option<String>,
    repo: bool,
}

impl Default for SourceEntry {
    fn default() -> Self {
        Self {
            path: String::new(),
            // A source is a repository unless it says otherwise: that is the
            // common case and the stronger change detector.
            name: None,
            repo: true,
        }
    }
}

// ─── Loading ────────────────────────────────────────────────────────

/// `~/.config/agent/config.toml`, the file `kx` and `[embed]` already use.
pub fn default_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/agent/config.toml"))
}

/// `~/.cache/code_index` — the one host location the family lives in.
pub fn default_cache_dir() -> PathBuf {
    expand_home("~/.cache/code_index")
}

/// Resolve a configured path against the user's home: `~`, `~/x`, and
/// `$HOME/x` expand; anything else is taken literally.
///
/// Config files and defaults must stay portable. A literal `/home/<someone>`
/// baked into the repo is wrong on the deploy host, wrong inside every jail,
/// and wrong on every other machine — so paths are written `~/...` and
/// resolved here, at the point of use, against the environment that is
/// actually running.
pub fn expand_home(raw: &str) -> PathBuf {
    let raw = raw.trim();
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    if raw == "~" {
        if let Some(h) = home() {
            return h;
        }
    }
    for prefix in ["~/", "$HOME/"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            if let Some(h) = home() {
                return h.join(rest);
            }
        }
    }
    PathBuf::from(raw)
}

impl Sources {
    /// Lenient load: a missing, unreadable, or malformed config yields an EMPTY
    /// source set rather than an error. Callers keep working exactly as they
    /// did before the section existed — configuring sources is an upgrade, not
    /// a precondition.
    pub fn load() -> Self {
        default_config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| Self::parse(&s).ok())
            .unwrap_or_else(|| Self::empty(default_cache_dir()))
    }

    /// Strict parse — used by tests and by tooling that wants to report a
    /// broken config instead of silently ignoring it.
    pub fn parse(toml_text: &str) -> Result<Self, toml::de::Error> {
        let root: RootConfig = toml::from_str(toml_text)?;
        let cache_dir = root
            .code_index
            .cache_dir
            .as_deref()
            .map(expand_home)
            .unwrap_or_else(default_cache_dir);

        let entries = root
            .code_index
            .sources
            .into_iter()
            .filter(|e| !e.path.trim().is_empty())
            .map(|e| {
                let path = expand_home(&e.path);
                let name = e.name.unwrap_or_else(|| basename_key(&path));
                Source {
                    path,
                    name,
                    repo: e.repo,
                }
            })
            .collect();

        Ok(Self { entries, cache_dir })
    }

    pub fn empty(cache_dir: PathBuf) -> Self {
        Self {
            entries: Vec::new(),
            cache_dir,
        }
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn entries(&self) -> &[Source] {
        &self.entries
    }

    pub fn get(&self, name: &str) -> Option<&Source> {
        self.entries.iter().find(|s| s.name == name)
    }

    /// Every key the service can serve right now: configured sources first
    /// (in config order), then any other populated db already in the cache
    /// directory. This is what an agent needs to stop guessing.
    pub fn available(&self) -> Vec<String> {
        let mut out: Vec<String> = self.entries.iter().map(|s| s.name.clone()).collect();
        for name in self.discovered() {
            if !out.contains(&name) {
                out.push(name);
            }
        }
        out
    }

    /// Populated `<name>.db` files present in the cache directory but not
    /// configured. These still resolve (no regression) — they are simply
    /// unmanaged: no cron keeps them fresh.
    pub fn discovered(&self) -> Vec<String> {
        let mut names: Vec<String> = match std::fs::read_dir(&self.cache_dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| is_populated(&e.path()))
                .filter_map(|e| {
                    e.path()
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(str::to_string)
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names
    }

    /// Resolve a `db` argument to a path that EXISTS, or explain why not.
    /// See the module docs for the four-step policy.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf, ResolveError> {
        let requested = requested.trim();

        // 1. Absolute, or `~`/`$HOME`-relative, is taken at its word — but it
        //    must exist.
        //
        //    Expanding FIRST is load-bearing, not cosmetic. An unexpanded
        //    tilde used to fall through to the bare-name branch and get joined
        //    under the cache dir, so `~/.cache/code_index/mu.db` resolved to
        //    `<cache_dir>/~/.cache/code_index/mu.db.db` — and the old
        //    create-on-open read path then MADE that whole tree. That is the
        //    literal `~` directory that kept reappearing in the cache and had
        //    to be deleted by hand.
        let expanded = expand_home(requested);
        if expanded.is_absolute() {
            return if expanded.is_file() {
                Ok(expanded)
            } else {
                Err(self.err(requested, Some(expanded)))
            };
        }

        // Reject anything that would escape the cache directory or name a
        // nested path; keys are single components by construction.
        if requested.is_empty() || requested.contains('/') || requested.contains("..") {
            return Err(self.err(requested, None));
        }

        // 2. A configured source name.
        if let Some(src) = self.get(requested) {
            let p = src.db_path(&self.cache_dir);
            return if p.is_file() {
                Ok(p)
            } else {
                Err(self.err(requested, Some(p)))
            };
        }

        // 3. Unconfigured, but already in the cache family — keep serving it.
        let p = self.cache_dir.join(format!("{requested}.db"));
        if p.is_file() {
            return Ok(p);
        }

        // 4. Nothing to open. Say so, with the path and the alternatives.
        Err(self.err(requested, Some(p)))
    }

    fn err(&self, requested: &str, resolved: Option<PathBuf>) -> ResolveError {
        ResolveError {
            requested: requested.to_string(),
            resolved,
            available: self.available(),
        }
    }
}

/// Is this a real index — a readable sqlite db with at least one chunk?
///
/// Deliberately NOT a size test. An open-created shell is ~74k of empty
/// schema, but so is the index of a genuinely small repo; only the row count
/// separates them. Getting this wrong in the cheap direction is what makes an
/// index that works fail to appear in the listing.
pub fn is_populated(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("db") {
        return false;
    }
    if !path.is_file() {
        return false;
    }
    chunk_count(path).is_some_and(|n| n > 0)
}

/// Chunk count for an index, or `None` if it cannot be opened/queried
/// (missing, not sqlite, or not a code-index schema).
pub fn chunk_count(path: &Path) -> Option<i64> {
    rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?
    .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
    .ok()
}

/// Default key for a source: the path's trailing component.
///
/// This is a per-machine accident (a checkout can be named anything) and is
/// what at-lcn replaces with a normalized origin URL. Until then, `name`
/// exists so a source can pin its key explicitly.
fn basename_key(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("index")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
[code_index]
cache_dir = "/tmp/ci-test-cache"

[[code_index.sources]]
path = "/src/public_github/mu"

[[code_index.sources]]
path = "/src/public_github/agent_tools"
repo = true

[[code_index.sources]]
path = "/src/notes"
repo = false
name = "notebook"
"#;

    #[test]
    fn parses_sources_with_defaults_and_overrides() {
        let s = Sources::parse(CFG).expect("parse");
        assert_eq!(s.cache_dir(), Path::new("/tmp/ci-test-cache"));
        assert_eq!(s.entries().len(), 3);

        // name defaults to the path basename; repo defaults to true
        let mu = s.get("mu").expect("mu configured");
        assert_eq!(mu.path, PathBuf::from("/src/public_github/mu"));
        assert!(mu.repo);

        // explicit name overrides the basename; repo=false is honored
        assert!(s.get("notes").is_none());
        let nb = s.get("notebook").expect("notebook configured");
        assert!(!nb.repo);
        assert_eq!(
            nb.db_path(s.cache_dir()),
            Path::new("/tmp/ci-test-cache/notebook.db")
        );
    }

    #[test]
    fn tilde_and_home_var_resolve_against_the_environment() {
        // The whole point: a config may say `~/...` so the repo never has to
        // name anyone's home directory. If this regresses, the shipped
        // config.toml.example becomes a lie and sources silently resolve to
        // a literal "~" directory.
        let home = PathBuf::from(std::env::var("HOME").expect("HOME set in tests"));
        let s = Sources::parse(
            r#"
[code_index]
cache_dir = "~/.cache/ci-test"

[[code_index.sources]]
path = "~/src/public_github/mu"

[[code_index.sources]]
path = "$HOME/src/public_github/agent_tools"

[[code_index.sources]]
path = "/absolute/stays/put"
name = "absolute"
"#,
        )
        .expect("parse");

        assert_eq!(s.cache_dir(), home.join(".cache/ci-test"));
        assert_eq!(
            s.get("mu").expect("mu").path,
            home.join("src/public_github/mu")
        );
        assert_eq!(
            s.get("agent_tools").expect("agent_tools").path,
            home.join("src/public_github/agent_tools")
        );
        // A genuinely absolute path is left exactly as written.
        assert_eq!(
            s.get("absolute").expect("absolute").path,
            PathBuf::from("/absolute/stays/put")
        );
        // And the derived key comes from the EXPANDED path, so `~/x/mu`
        // keys as "mu", not as something containing a tilde.
        assert!(s.get("mu").is_some());
    }

    #[test]
    fn missing_section_yields_empty_not_error() {
        let s = Sources::parse("[embed]\nmodel = \"x\"\n").expect("parse");
        assert!(s.entries().is_empty());
    }

    /// REGRESSION: a `db` argument written with a tilde must never be treated
    /// as a bare NAME and joined under the cache dir.
    ///
    /// That is what produced the literal `~` directory the operator kept
    /// having to delete: `~/.cache/code_index/mu.db` resolved to
    /// `<cache_dir>/~/.cache/code_index/mu.db.db`, and the old create-on-open
    /// read path then made the whole tree.
    #[test]
    fn tilde_db_argument_never_lands_under_the_cache_dir() {
        let dir = std::env::temp_dir().join("ci-src-tilde-db-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let s = Sources::empty(dir.clone());

        for req in [
            "~/.cache/code_index/mu.db",
            "$HOME/.cache/code_index/mu.db",
            "~",
        ] {
            match s.resolve(req) {
                // Fine if it resolves — but only to a real, absolute file.
                Ok(p) => {
                    assert!(p.is_absolute(), "{req} resolved to a relative path: {p:?}");
                    assert!(!p.starts_with(&dir), "{req} was joined under the cache dir");
                }
                // Fine if it errors — but the reported path must not be the
                // cache-dir-joined monstrosity either.
                Err(e) => {
                    if let Some(p) = &e.resolved {
                        assert!(
                            !p.components().any(|c| c.as_os_str() == "~"),
                            "{req} produced a literal `~` component: {p:?}"
                        );
                    }
                }
            }
        }

        // And nothing may have been created on disk by asking.
        let created: Vec<_> = std::fs::read_dir(&dir)
            .expect("read cache dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert!(created.is_empty(), "resolve created entries: {created:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_rejects_traversal_and_empty() {
        let s = Sources::parse(CFG).expect("parse");
        for bad in ["", "../etc/passwd", "a/b", ".."] {
            assert!(s.resolve(bad).is_err(), "{bad:?} should not resolve");
        }
    }

    #[test]
    fn resolve_error_names_the_path_and_alternatives() {
        let s = Sources::parse(CFG).expect("parse");
        let e = s.resolve("mu").expect_err("no db on disk in this test");
        assert_eq!(e.requested, "mu");
        assert_eq!(
            e.resolved.as_deref(),
            Some(Path::new("/tmp/ci-test-cache/mu.db"))
        );
        assert!(e.available.contains(&"mu".to_string()));
        // The rendered message must name the path — that is the whole point.
        assert!(e.to_string().contains("/tmp/ci-test-cache/mu.db"));
    }

    /// Build a code-index-shaped sqlite db with `chunks` rows.
    fn make_index(path: &Path, chunks: usize) {
        let conn = rusqlite::Connection::open(path).expect("create db");
        conn.execute_batch("CREATE TABLE chunks (id INTEGER PRIMARY KEY);")
            .expect("schema");
        for i in 0..chunks {
            conn.execute("INSERT INTO chunks (id) VALUES (?)", [i as i64])
                .expect("insert");
        }
    }

    #[test]
    fn unconfigured_but_present_db_still_resolves_and_lists() {
        let dir = std::env::temp_dir().join("ci-src-legacy-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let legacy = dir.join("codex.db");
        make_index(&legacy, 3);

        let s = Sources::empty(dir.clone());
        // Nothing is configured, yet the existing cache member keeps working —
        // adding config must never take away a db that resolves today.
        assert_eq!(s.resolve("codex").expect("legacy resolves"), legacy);
        assert!(s.available().contains(&"codex".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn small_but_real_index_is_populated() {
        let dir = std::env::temp_dir().join("ci-src-small-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let small = dir.join("tiny.db");
        make_index(&small, 1);

        // Far under the old 100k size threshold, but it is a REAL index and
        // must be listed — judging by file size hid working indexes.
        assert!(small.metadata().expect("stat").len() < MIN_POPULATED_DB_SIZE);
        let s = Sources::empty(dir.clone());
        assert_eq!(s.discovered(), vec!["tiny".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_created_shell_is_not_listed() {
        let dir = std::env::temp_dir().join("ci-src-empty-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Exactly what a query-path open-create used to leave behind: valid
        // sqlite, correct schema, zero rows.
        make_index(&dir.join("ghost.db"), 0);
        // And a file that is not sqlite at all.
        std::fs::write(dir.join("junk.db"), vec![0u8; 200_000]).expect("write");

        let s = Sources::empty(dir.clone());
        assert!(s.discovered().is_empty());
        assert!(!s.available().contains(&"ghost".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
