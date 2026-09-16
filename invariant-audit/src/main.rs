//! invariant-audit — enumerate every site of a violation shape; ratchet the count.
//!
//! The installed form of mu's `scripts/invariant-audit.py` (mu #628), same
//! contract, with test regions read off the tree-sitter parse tree
//! (`tree-regions`) instead of a lexer. agent_tools bead at-zzb.
//!
//! A correction applied to one site of an anti-pattern leaves the others, and
//! whatever pattern exists in the code is what the next change copies. This
//! tool makes an invariant mechanical: `.invariants.toml` describes each one
//! as a violation SHAPE (a regex over a path scope, a bare path glob, or a
//! Cargo dependency) plus the number of sites the repo has today (`baseline`).
//! The audit lists every site and fails when any count rises above its
//! baseline. Baselines only go down: when a change removes sites, lower the
//! number in the same change (the audit prints the hint).
//!
//! Baselines are pinned to BASE (default `main`): a change cannot raise a
//! baseline in the commit that adds the violation. The effective ceiling is
//! the lower of BASE's and the checkout's; an invariant absent from BASE is new
//! and initialises at the checkout's value. The SHAPE is pinned as well as the
//! number: when a checkout changes an invariant's shape, BASE's shape is still
//! counted against this checkout and held to BASE's ceiling, so narrowing a
//! shape cannot hide a new site. Fewer sites than the checkout's baseline is
//! also a failure in gate mode, so the recorded count is always the true one.
//! An invariant present at BASE may not silently disappear: keep its entry
//! with `retired = true`. `[settings].exclude` is environmental: the BASE pass
//! walks with the union of BASE's and the checkout's, so an exclusion added for
//! this machine (a venv, a symlink) can pass its own gate; shape-level
//! `exclude` stays pinned to BASE (a narrowing guard).
//!
//! Exit codes: 0 clean, 1 ratchet failure, 2 the audit could not run (bad
//! shapes file, bad regex, unreadable or unparseable scoped file, BASE
//! requested but unresolvable).
//!
//! Shapes file:
//!   [settings]
//!   exclude = [".git/**", ".jj/**", "target/**"]   # default excludes
//!   [[invariant]]
//!   id = "short-id"
//!   rule = "the sentence from AGENTS.md"
//!   kind = "content"            # "path", or "cargo-dependency"
//!   paths = ["crates/**/*.rs"]  # root-relative globs; `x.md` is the root file
//!   exclude = ["**/tests/**"]   # optional, per invariant
//!   pattern = 'regex'           # content kind only
//!   ignore_case = false         # content kind only
//!   skip_cfg_test = false       # content kind, Rust files: drop matches inside a
//!                               # `#[cfg(test)]`-attributed item (the parse tree
//!                               # decides); the skipped count is printed
//!   crate = "mu-core"           # cargo-dependency kind only
//!   baseline = 0                # sites the repo has today
//!   retired = false

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};

mod cache;
mod cargo_dep;
mod glob;

const DEFAULT_EXCLUDES: &[&str] = &[".git/**", ".jj/**", "target/**"];

#[derive(Parser, Debug)]
#[command(
    name = "invariant-audit",
    about = "enumerate every site of each violation shape and ratchet the count"
)]
struct Args {
    /// Repository root (default: the current directory's jj/git root, else cwd).
    #[arg(long)]
    root: Option<PathBuf>,
    /// Shapes file (default <root>/.invariants.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// List every site; never fail.
    #[arg(long)]
    report: bool,
    /// Revision whose shapes file pins the baselines (default $MU_INVARIANTS_BASE or "main").
    #[arg(long, env = "MU_INVARIANTS_BASE", default_value = "main")]
    base: String,
    /// BASE's shapes file, already extracted (CI).
    #[arg(long)]
    base_config: Option<PathBuf>,
    /// Use the checkout's own baselines (first commit, push to main, throwaway repos).
    #[arg(long)]
    no_base: bool,
    /// Do not read or write the cfg(test) region cache.
    #[arg(long)]
    no_cache: bool,
}

/// One validated `[[invariant]]`.
#[derive(Debug, Clone, PartialEq)]
struct Invariant {
    id: String,
    rule: String,
    kind: Kind,
    paths: Vec<String>,
    exclude: Vec<String>,
    pattern: Option<String>,
    ignore_case: bool,
    skip_cfg_test: bool,
    krate: Option<String>,
    baseline: i64,
    retired: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Content,
    Path,
    CargoDependency,
}

/// The pinned shape of an invariant: everything but the id, rule, baseline
/// and retired flag. Two entries with equal shapes count the same sites.
#[derive(Debug, PartialEq)]
struct Shape<'a> {
    kind: Kind,
    paths: &'a [String],
    exclude: &'a [String],
    pattern: Option<&'a str>,
    ignore_case: bool,
    skip_cfg_test: bool,
    krate: Option<&'a str>,
}

impl Invariant {
    fn shape(&self) -> Shape<'_> {
        Shape {
            kind: self.kind,
            paths: &self.paths,
            exclude: &self.exclude,
            pattern: self.pattern.as_deref(),
            ignore_case: self.ignore_case,
            skip_cfg_test: self.skip_cfg_test,
            krate: self.krate.as_deref(),
        }
    }
}

/// A site: root-relative path, 1-based line (None for path/dependency kinds),
/// and the trimmed text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Site {
    rel: String,
    line: Option<usize>,
    text: String,
}

impl Site {
    fn loc(&self) -> String {
        match self.line {
            Some(n) => format!("{}:{n}", self.rel),
            None => self.rel.clone(),
        }
    }
}

fn str_list(value: &toml::Value, what: &str) -> Result<Vec<String>> {
    let arr = value.as_array().ok_or_else(|| {
        anyhow!(
            "{what} must be a list of non-empty strings (got {})",
            value.type_str()
        )
    })?;
    arr.iter()
        .map(|v| match v.as_str() {
            Some(s) if !s.is_empty() => Ok(s.to_string()),
            _ => bail!("{what} must be a list of non-empty strings"),
        })
        .collect()
}

fn parse_shapes(text: &str, what: &str) -> Result<(Vec<String>, Vec<Invariant>)> {
    let doc: toml::Value =
        toml::from_str(text).with_context(|| format!("{what}: not valid TOML"))?;
    let table = doc
        .as_table()
        .ok_or_else(|| anyhow!("{what}: not a TOML table"))?;
    let excludes = match table.get("settings") {
        None => DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect(),
        Some(s) => {
            let st = s
                .as_table()
                .ok_or_else(|| anyhow!("{what}: [settings] must be a table"))?;
            match st.get("exclude") {
                None => DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect(),
                Some(v) => str_list(v, &format!("{what}: settings.exclude"))?,
            }
        }
    };
    for g in &excludes {
        glob::compile(g).with_context(|| format!("{what}: settings.exclude"))?;
    }
    let raw = match table.get("invariant") {
        None => Vec::new(),
        Some(v) => v
            .as_array()
            .ok_or_else(|| {
                anyhow!(
                    "{what}: invariant must be an array of tables ([[invariant]]), got {}",
                    v.type_str()
                )
            })?
            .clone(),
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for v in raw {
        let t = v
            .as_table()
            .ok_or_else(|| anyhow!("{what}: every [[invariant]] must be a table"))?;
        for key in ["id", "rule", "paths"] {
            if !t.contains_key(key) {
                bail!("{what}: an [[invariant]] is missing required key {key:?}");
            }
        }
        let get_str = |key: &str| -> Result<String> {
            match t.get(key).and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => Ok(s.to_string()),
                _ => bail!("{what}: invariant {key} must be a non-empty string"),
            }
        };
        let id = get_str("id")?;
        let rule = get_str("rule")?;
        if !seen.insert(id.clone()) {
            bail!("{what}: duplicate invariant id {id:?}");
        }
        let kind = match t.get("kind") {
            None => Kind::Content,
            Some(k) => match k.as_str() {
                Some("content") => Kind::Content,
                Some("path") => Kind::Path,
                Some("cargo-dependency") => Kind::CargoDependency,
                Some(other) => bail!(
                    "{what}: invariant {id:?}: unknown kind {other:?} (content|path|cargo-dependency)"
                ),
                None => bail!("{what}: invariant {id:?}: kind must be a string"),
            },
        };
        let paths = str_list(&t["paths"], &format!("{what}: invariant {id:?}: paths"))?;
        if paths.is_empty() {
            bail!("{what}: invariant {id:?}: paths must name at least one glob");
        }
        let exclude = match t.get("exclude") {
            None => Vec::new(),
            Some(v) => str_list(v, &format!("{what}: invariant {id:?}: exclude"))?,
        };
        for g in paths.iter().chain(exclude.iter()) {
            glob::compile(g).with_context(|| format!("{what}: invariant {id:?}"))?;
        }
        let bool_key = |key: &str| -> Result<bool> {
            match t.get(key) {
                None => Ok(false),
                Some(v) => v
                    .as_bool()
                    .ok_or_else(|| anyhow!("{what}: invariant {id:?}: {key} must be true/false")),
            }
        };
        let ignore_case = bool_key("ignore_case")?;
        let skip_cfg_test = bool_key("skip_cfg_test")?;
        let retired = bool_key("retired")?;
        let pattern = match t.get("pattern") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .ok_or_else(|| anyhow!("{what}: invariant {id:?}: pattern must be a string"))?
                    .to_string(),
            ),
        };
        let krate = match t.get("crate") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow!("{what}: invariant {id:?}: crate must be a non-empty string")
                    })?
                    .to_string(),
            ),
        };
        match kind {
            Kind::CargoDependency if krate.is_none() => {
                bail!("{what}: invariant {id:?} is kind=cargo-dependency but has no crate")
            }
            Kind::Content => {
                let p = pattern.as_deref().ok_or_else(|| {
                    anyhow!("{what}: invariant {id:?} is kind=content but has no pattern")
                })?;
                RegexBuilder::new(p)
                    .case_insensitive(ignore_case)
                    .build()
                    .with_context(|| format!("{what}: invariant {id:?}: bad regex"))?;
            }
            _ => {}
        }
        if skip_cfg_test && kind != Kind::Content {
            bail!("{what}: invariant {id:?}: skip_cfg_test applies to kind=content only");
        }
        let baseline = match t.get("baseline") {
            None => 0,
            Some(v) => v
                .as_integer()
                .ok_or_else(|| anyhow!("{what}: invariant {id:?}: baseline must be an integer"))?,
        };
        out.push(Invariant {
            id,
            rule,
            kind,
            paths,
            exclude,
            pattern,
            ignore_case,
            skip_cfg_test,
            krate,
            baseline,
            retired,
        });
    }
    Ok((excludes, out))
}

/// Every file under root as a posix relative path, pruning excluded
/// directories. An unreadable directory or a directory symlink in scope is an
/// error: a gate that cannot see part of its scope must not pass.
fn walk(root: &Path, excludes: &[String]) -> Result<Vec<String>> {
    let ex: Vec<globset::GlobMatcher> = excludes
        .iter()
        .map(|g| glob::compile(g))
        .collect::<Result<_>>()?;
    let excluded = |rel: &str| ex.iter().any(|m| m.is_match(rel));
    let mut files = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(rel_dir) = stack.pop() {
        let dir = if rel_dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&rel_dir)
        };
        let rd = fs::read_dir(&dir).with_context(|| {
            format!(
                "cannot enumerate scoped directory {}",
                if rel_dir.is_empty() { "." } else { &rel_dir }
            )
        })?;
        let mut entries: Vec<(String, fs::FileType)> = Vec::new();
        for e in rd {
            let e = e.with_context(|| format!("cannot enumerate scoped directory {rel_dir}"))?;
            let name = e.file_name().to_string_lossy().into_owned();
            let ft = e.file_type()?;
            entries.push((name, ft));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, ft) in entries {
            let rel = if rel_dir.is_empty() {
                name.clone()
            } else {
                format!("{rel_dir}/{name}")
            };
            if ft.is_dir() {
                if excluded(&rel) || excluded(&format!("{rel}/")) {
                    continue;
                }
                stack.push(rel);
            } else if ft.is_symlink() {
                let target_is_dir = fs::metadata(root.join(&rel))
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
                if target_is_dir {
                    if excluded(&rel) || excluded(&format!("{rel}/")) {
                        continue;
                    }
                    bail!(
                        "scoped directory {rel} is a symlink; the audit does not follow directory symlinks (exclude it in [settings] or replace it)"
                    );
                }
                if !excluded(&rel) {
                    files.push(rel);
                }
            } else if !excluded(&rel) {
                files.push(rel);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn scoped<'a>(all: &'a [String], globs: &[String], exclude: &[String]) -> Result<Vec<&'a String>> {
    let inc: Vec<_> = globs
        .iter()
        .map(|g| glob::compile(g))
        .collect::<Result<_>>()?;
    let exc: Vec<_> = exclude
        .iter()
        .map(|g| glob::compile(g))
        .collect::<Result<_>>()?;
    Ok(all
        .iter()
        .filter(|rel| inc.iter().any(|m| m.is_match(rel)) && !exc.iter().any(|m| m.is_match(rel)))
        .collect())
}

/// (every site, sites skipped as `#[cfg(test)]` regions). The skipped count
/// is zero unless the invariant sets `skip_cfg_test`.
fn sites(
    root: &Path,
    all: &[String],
    inv: &Invariant,
    cache: &cache::RegionCache,
) -> Result<(Vec<Site>, usize)> {
    let files = scoped(all, &inv.paths, &inv.exclude)?;
    match inv.kind {
        Kind::Path => Ok((
            files
                .into_iter()
                .map(|rel| Site {
                    rel: rel.clone(),
                    line: None,
                    text: String::new(),
                })
                .collect(),
            0,
        )),
        Kind::CargoDependency => {
            let krate = inv.krate.as_deref().expect("validated");
            let mut out = Vec::new();
            for rel in files {
                let text = fs::read(root.join(rel))
                    .with_context(|| format!("cannot read scoped file {rel}"))?;
                let text =
                    String::from_utf8(text).map_err(|e| anyhow!("{rel}: not valid UTF-8 ({e})"))?;
                out.extend(cargo_dep::sites(root, rel, &text, krate)?);
            }
            Ok((out, 0))
        }
        Kind::Content => {
            let rx = RegexBuilder::new(inv.pattern.as_deref().expect("validated"))
                .case_insensitive(inv.ignore_case)
                .build()?;
            let per_file: Vec<Result<(Vec<Site>, usize)>> = files
                .par_iter()
                .map(|rel| content_sites(root, rel, &rx, inv.skip_cfg_test, cache))
                .collect();
            let mut out = Vec::new();
            let mut skipped = 0;
            for r in per_file {
                let (s, k) = r?;
                out.extend(s);
                skipped += k;
            }
            Ok((out, skipped))
        }
    }
}

fn content_sites(
    root: &Path,
    rel: &str,
    rx: &Regex,
    skip_cfg_test: bool,
    cache: &cache::RegionCache,
) -> Result<(Vec<Site>, usize)> {
    let bytes =
        fs::read(root.join(rel)).with_context(|| format!("cannot read scoped file {rel}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let regions = if skip_cfg_test && rel.ends_with(".rs") {
        Some(cache.regions_for(rel, &bytes)?)
    } else {
        None
    };
    let mut out = Vec::new();
    let mut skipped = 0;
    let mut offset = 0usize;
    for (n, line) in text.split_inclusive('\n').enumerate() {
        let line_no = n + 1;
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        let matches: Vec<usize> = rx.find_iter(content).map(|m| offset + m.start()).collect();
        if !matches.is_empty() {
            // one site per line; the line is a site if ANY match on it lies
            // outside every test region
            let all_in = regions
                .as_ref()
                .map(|rs| matches.iter().all(|&b| rs.iter().any(|r| r.contains(b))))
                .unwrap_or(false);
            if all_in {
                skipped += 1;
            } else {
                let mut t: String = content.trim().chars().take(160).collect();
                if t.chars().count() >= 160 {
                    t.push('…');
                }
                out.push(Site {
                    rel: rel.to_string(),
                    line: Some(line_no),
                    text: t,
                });
            }
        }
        offset += line.len();
    }
    Ok((out, skipped))
}

fn run(cmd: &mut Command, root: &Path) -> Option<std::process::Output> {
    cmd.current_dir(root).output().ok()
}

/// Contents of `rel` at `rev`; `Ok(None)` only when the revision resolves and
/// the file is verifiably absent there. Any other failure is an error: a gate
/// that cannot see BASE must not fall back to the baselines it is checking.
fn file_at_rev(root: &Path, rev: &str, rel: &str) -> Result<Option<String>> {
    let absent = [
        "No such path",
        "does not exist",
        "exists on disk, but not in",
        "Path not found",
    ];
    let absent_in = |stderr: &str| absent.iter().any(|m| stderr.contains(m));
    if let Some(o) = run(
        Command::new("jj").args(["log", "-r", rev, "--no-graph", "-T", "commit_id"]),
        root,
    ) {
        if o.status.success() && !o.stdout.is_empty() {
            let o2 = run(
                Command::new("jj").args(["file", "show", "-r", rev, "--", rel]),
                root,
            )
            .ok_or_else(|| anyhow!("jj could not run"))?;
            if o2.status.success() {
                return Ok(Some(String::from_utf8_lossy(&o2.stdout).into_owned()));
            }
            let err = String::from_utf8_lossy(&o2.stderr);
            if absent_in(&err) {
                return Ok(None);
            }
            bail!(
                "jj could not read {rel} at {rev}: {}",
                err.trim().chars().take(200).collect::<String>()
            );
        }
    }
    if let Some(o) = run(
        Command::new("git").args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ]),
        root,
    ) {
        if o.status.success() {
            let o2 = run(
                Command::new("git").args(["show", &format!("{rev}:{rel}")]),
                root,
            )
            .ok_or_else(|| anyhow!("git could not run"))?;
            if o2.status.success() {
                return Ok(Some(String::from_utf8_lossy(&o2.stdout).into_owned()));
            }
            let err = String::from_utf8_lossy(&o2.stderr);
            if absent_in(&err) {
                return Ok(None);
            }
            bail!(
                "git could not read {rel} at {rev}: {}",
                err.trim().chars().take(200).collect::<String>()
            );
        }
    }
    bail!("cannot resolve BASE revision {rev:?} with jj or git; pass --base-config or --no-base deliberately")
}

struct Base {
    excludes: Vec<String>,
    invariants: BTreeMap<String, Invariant>,
}

fn load_base(
    root: &Path,
    config: &Path,
    base_rev: Option<&str>,
    base_config: Option<&Path>,
) -> Result<(Option<Base>, String)> {
    let (text, src, config_name) = if let Some(bc) = base_config {
        let t = fs::read_to_string(bc)
            .with_context(|| format!("cannot read BASE shapes file {}", bc.display()))?;
        (
            Some(t),
            format!("baselines pinned to {}", bc.display()),
            bc.display().to_string(),
        )
    } else if let Some(rev) = base_rev {
        let name = config
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .map_err(|_| anyhow!("--config {} is outside the repository root {}; pass --base-config or --no-base", config.display(), root.display()))?;
        (
            file_at_rev(root, rev, &name)?,
            format!("baselines pinned to BASE {rev}"),
            name,
        )
    } else {
        (
            None,
            String::new(),
            config
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
    };
    let Some(text) = text else {
        let why = match base_rev {
            None if base_config.is_none() => "no BASE requested".to_string(),
            Some(rev) => format!("no {config_name} at BASE {rev}"),
            None => "no BASE requested".to_string(),
        };
        return Ok((
            None,
            format!("{why}: using the checkout's own baselines (first commit / push to main)"),
        ));
    };
    let (excludes, invs) = parse_shapes(&text, "BASE shapes file")?;
    Ok((
        Some(Base {
            excludes,
            invariants: invs.into_iter().map(|i| (i.id.clone(), i)).collect(),
        }),
        src,
    ))
}

fn union(a: &[String], b: &[String]) -> Vec<String> {
    let mut out = a.to_vec();
    for s in b {
        if !out.contains(s) {
            out.push(s.clone());
        }
    }
    out
}

fn audit(
    root: &Path,
    config: &Path,
    report: bool,
    base_rev: Option<&str>,
    base_config: Option<&Path>,
    use_cache: bool,
) -> Result<i32> {
    let text =
        fs::read_to_string(config).with_context(|| format!("cannot read {}", config.display()))?;
    let (excludes, invs) = parse_shapes(&text, &config.display().to_string())?;
    let (base, base_src) = load_base(root, config, base_rev, base_config)?;
    println!("{base_src}");
    let cache = cache::RegionCache::open(root, use_cache);
    let mut failures = 0;
    let head_ids: std::collections::HashSet<&str> = invs.iter().map(|i| i.id.as_str()).collect();
    let (base_invs, base_excludes): (BTreeMap<String, Invariant>, Vec<String>) = match &base {
        Some(b) => (b.invariants.clone(), b.excludes.clone()),
        None => (BTreeMap::new(), Vec::new()),
    };
    for (bid, binv) in &base_invs {
        if !head_ids.contains(bid.as_str()) && !binv.retired {
            println!(
                "{bid}: VIOLATION — present at BASE but missing here; keep the entry with `retired = true` (say why in `rule`) instead of deleting it"
            );
            failures += 1;
        }
    }
    // [settings].exclude is environmental: walk once with the union so that an
    // exclusion this checkout needs applies to the BASE pass too (05uy1);
    // shape-level exclude stays each shape's own.
    let walk_excludes = if base.is_some() {
        union(&excludes, &base_excludes)
    } else {
        excludes.clone()
    };
    let all_files = walk(root, &walk_excludes)?;
    for inv in &invs {
        let head_b = inv.baseline;
        if inv.retired {
            println!(
                "{}: retired — not counted ({})",
                inv.id,
                inv.rule.chars().take(120).collect::<String>()
            );
            continue;
        }
        let mut notes: Vec<String> = Vec::new();
        let mut ceiling = head_b;
        let binv = base_invs.get(&inv.id);
        let pinned = binv.map(|b| !b.retired).unwrap_or(false);
        if base.is_some() && binv.is_none() {
            notes.push(format!(
                "new invariant (not at BASE): baseline initialised at {head_b}"
            ));
        } else if let Some(b) = binv.filter(|b| b.retired) {
            let _ = b;
            notes.push(format!(
                "re-activated (retired at BASE): baseline initialised at {head_b}"
            ));
        }
        let (found, skipped) = sites(root, &all_files, inv, &cache)?;
        let n = found.len() as i64;
        if pinned {
            let b = binv.expect("pinned");
            let base_b = b.baseline;
            if b.shape() == inv.shape() {
                if head_b > base_b {
                    notes.push(format!("VIOLATION: baseline raised from {base_b} (BASE) to {head_b}; baselines only go down"));
                    failures += 1;
                }
                ceiling = head_b.min(base_b);
            } else {
                let (base_sites, _) = sites(root, &all_files, b, &cache)?;
                let n_base = base_sites.len() as i64;
                notes.push(format!("shape changed since BASE; BASE's shape still counts {n_base} site(s) here against its ceiling {base_b}"));
                if n_base > base_b {
                    notes.push(format!("VIOLATION: {n_base} site(s) under BASE's shape exceed BASE's baseline {base_b}"));
                    for s in &base_sites {
                        notes.push(format!(
                            "  [BASE shape] {}{}",
                            s.loc(),
                            if s.text.is_empty() {
                                String::new()
                            } else {
                                format!(": {}", s.text)
                            }
                        ));
                    }
                    failures += 1;
                }
                ceiling = head_b;
            }
        }
        let status = if n > ceiling {
            failures += 1;
            "VIOLATION"
        } else if n < head_b {
            failures += 1;
            "stale baseline"
        } else {
            "ok"
        };
        let skipped_note = if inv.skip_cfg_test {
            format!(" ({skipped} in #[cfg(test)] skipped)")
        } else {
            String::new()
        };
        println!(
            "{}: {n} site(s){skipped_note}, baseline {head_b} — {status}",
            inv.id
        );
        for note in &notes {
            println!("  {note}");
        }
        if status != "ok" || report || notes.iter().any(|x| x.starts_with("VIOLATION")) {
            println!("  rule: {}", inv.rule);
            for s in &found {
                println!(
                    "  {}{}",
                    s.loc(),
                    if s.text.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", s.text)
                    }
                );
            }
        }
        if n < head_b {
            let cfg_name = config
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            println!(
                "  hint: sites were removed — lower `baseline` for {:?} to {n} in {cfg_name}{}",
                inv.id,
                if report {
                    ""
                } else {
                    " (a stale baseline fails the gate)"
                }
            );
        }
    }
    cache.flush();
    let cfg_name = config
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if failures > 0 && !report {
        println!(
            "\ninvariant-audit: {failures} failure(s) — fix every listed site, lower a stale baseline, or argue the shape in {cfg_name}; baselines are only ever lowered."
        );
        return Ok(1);
    }
    println!(
        "\ninvariant-audit: {} invariant(s) checked, {failures} would fail{}",
        invs.len(),
        if report && failures > 0 {
            " (report mode: not failing)"
        } else {
            ""
        }
    );
    Ok(0)
}

fn repo_root(start: &Path) -> PathBuf {
    let mut d = start.to_path_buf();
    loop {
        if d.join(".jj").is_dir() || d.join(".git").exists() {
            return d;
        }
        match d.parent() {
            Some(p) => d = p.to_path_buf(),
            None => return start.to_path_buf(),
        }
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = args.root.clone().unwrap_or_else(|| repo_root(&cwd));
    let root = root.canonicalize().unwrap_or(root);
    let config = args
        .config
        .clone()
        .unwrap_or_else(|| root.join(".invariants.toml"));
    let config = config.canonicalize().unwrap_or(config);
    if !config.is_file() {
        eprintln!("invariant-audit: no shapes file at {}", config.display());
        return ExitCode::from(2);
    }
    let base_rev = if args.no_base {
        None
    } else {
        Some(args.base.as_str())
    };
    match audit(
        &root,
        &config,
        args.report,
        base_rev,
        args.base_config.as_deref(),
        !args.no_cache,
    ) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("invariant-audit: {e:#}");
            ExitCode::from(2)
        }
    }
}
