//! `kind = "cargo-dependency"`: every dependency whose RESOLVED package is the
//! named crate, in one parsed Cargo manifest. The key names the package unless
//! `package = ...` renames it; `workspace = true` inherits both from the
//! nearest workspace's `[workspace.dependencies]`. Covers `[dependencies]`,
//! `[dev-dependencies]`, `[build-dependencies]`, the deprecated underscore
//! spellings, and target-specific tables. A parsed manifest has no comments
//! and no quoting, so none of those matter.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use toml::Value;

use crate::Site;

const TABLES: &[&str] = &[
    "dependencies",
    "dev-dependencies",
    "build-dependencies",
    "dev_dependencies",
    "build_dependencies",
];

fn workspace_dependencies(root: &Path, manifest_rel: &str, doc: &Value) -> Result<toml::Table> {
    if let Some(ws) = doc.get("workspace").and_then(|w| w.as_table()) {
        return Ok(ws
            .get("dependencies")
            .and_then(|d| d.as_table())
            .cloned()
            .unwrap_or_default());
    }
    let manifest = root.join(manifest_rel);
    // An explicit [package].workspace = "<dir>" selects a root that need not be
    // an ancestor; Cargo resolves it relative to the package's directory.
    if let Some(sel) = doc
        .get("package")
        .and_then(|p| p.get("workspace"))
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
    {
        let cand = manifest
            .parent()
            .unwrap_or(root)
            .join(sel)
            .join("Cargo.toml");
        let cand = cand.canonicalize().unwrap_or(cand);
        let root_c = root.canonicalize().unwrap_or(root.to_path_buf());
        if !cand.starts_with(&root_c) {
            bail!("{manifest_rel}: [package].workspace = {sel:?} selects a root outside the repository ({}); refusing to read it", cand.display());
        }
        let rel = cand
            .strip_prefix(&root_c)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let text = fs::read_to_string(&cand)
            .with_context(|| format!("cannot read selected workspace root {rel}"))?;
        let wdoc: Value = toml::from_str(&text).with_context(|| {
            format!("{rel}: selected workspace root is not a valid Cargo manifest")
        })?;
        return Ok(wdoc
            .get("workspace")
            .and_then(|w| w.get("dependencies"))
            .and_then(|d| d.as_table())
            .cloned()
            .unwrap_or_default());
    }
    let mut d = manifest.parent().map(|p| p.to_path_buf());
    while let Some(dir) = d {
        let cand = dir.join("Cargo.toml");
        if cand != manifest && cand.is_file() {
            let rel = cand
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| cand.display().to_string());
            let text = fs::read_to_string(&cand).with_context(|| format!("cannot read {rel}"))?;
            let wdoc: Value = toml::from_str(&text)
                .with_context(|| format!("{rel}: not a valid Cargo manifest"))?;
            if let Some(ws) = wdoc.get("workspace").and_then(|w| w.as_table()) {
                return Ok(ws
                    .get("dependencies")
                    .and_then(|d| d.as_table())
                    .cloned()
                    .unwrap_or_default());
            }
        }
        if dir == root {
            break;
        }
        d = dir.parent().map(|p| p.to_path_buf());
    }
    Ok(toml::Table::new())
}

pub fn sites(root: &Path, rel: &str, text: &str, krate: &str) -> Result<Vec<Site>> {
    let doc: Value =
        toml::from_str(text).map_err(|e| anyhow!("{rel}: not a valid Cargo manifest: {e}"))?;
    let mut tables: Vec<(String, toml::Table)> = Vec::new();
    for name in TABLES {
        if let Some(t) = doc.get(*name).and_then(|v| v.as_table()) {
            tables.push((name.to_string(), t.clone()));
        }
    }
    if let Some(targets) = doc.get("target").and_then(|v| v.as_table()) {
        for (tgt, spec) in targets {
            if let Some(spec) = spec.as_table() {
                for name in TABLES {
                    if let Some(t) = spec.get(*name).and_then(|v| v.as_table()) {
                        tables.push((format!("target.{tgt}.{name}"), t.clone()));
                    }
                }
            }
        }
    }
    let mut ws_deps: Option<toml::Table> = None;
    let mut out = Vec::new();
    for (table, deps) in tables {
        for (key, spec) in &deps {
            let inherited = spec.get("workspace").and_then(|w| w.as_bool()) == Some(true);
            let package: Option<String> = if inherited {
                // The workspace entry is authoritative for an inherited
                // dependency; a member-local `package` next to `workspace =
                // true` is not valid Cargo and must not mask the identity.
                if ws_deps.is_none() {
                    ws_deps = Some(workspace_dependencies(root, rel, &doc)?);
                }
                ws_deps
                    .as_ref()
                    .unwrap()
                    .get(key)
                    .and_then(|s| s.get("package"))
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string())
            } else {
                spec.get("package")
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string())
            };
            let resolved = package.clone().unwrap_or_else(|| key.clone());
            if resolved == krate {
                let how = match &package {
                    None => key.clone(),
                    Some(p) => format!(
                        "{key} = {{ package = {p:?}{} }}",
                        if inherited { ", workspace = true" } else { "" }
                    ),
                };
                out.push(Site {
                    rel: rel.to_string(),
                    line: None,
                    text: format!("[{table}] {how}"),
                });
            }
        }
    }
    Ok(out)
}
