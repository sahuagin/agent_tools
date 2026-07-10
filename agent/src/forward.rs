//! `--via-mcp <url>` / `AGENT_MCP_URL` / config-default `[agent] mcp_url`:
//! forward an `agent` invocation to a remote agent-mcp as an MCP `tools/call`,
//! instead of opening the local DB. Only the DB-backed groups (memory/task/
//! metrics) route; `dialogue` and `db-path` always stay local.
//!
//! This is the inverse of agent-mcp's argv builder: `["memory","recall","x",
//! "--k","6"]` → tool `memory_recall`, arguments `{"query":"x","k":"6"}`. It
//! uses a synchronous `ureq` client — no async runtime is pulled into `agent`.
//! Loop-safe: agent-mcp sets `AGENT_NO_FORWARD` on the inner `agent` it spawns,
//! so the server-side backend always uses the local DB and never re-forwards.

use std::collections::HashMap;
use std::io::Read;

use anyhow::{anyhow, bail, Context, Result};
use clap::CommandFactory;
use serde_json::{Map, Value};

use crate::Cli;

/// Resolve the forward target: `Some((url, subcommand-argv))` if the CLI should
/// forward to a remote agent-mcp instead of opening the local DB, else `None`
/// (run locally). Precedence: `--via-mcp <url>` flag > `AGENT_MCP_URL` env >
/// config `[agent] mcp_url` > local DB. The config-default routes to the central
/// store by default so memory/tasks/metrics stop drifting per-machine.
pub fn target(argv: &[String]) -> Option<(String, Vec<String>)> {
    // Loop-guard: agent-mcp sets AGENT_NO_FORWARD on the inner `agent` it spawns,
    // so that backend always opens the LOCAL DB and never re-forwards — else
    // `agent(client) → agent-mcp → agent(server) → agent-mcp → …` would loop.
    if std::env::var_os("AGENT_NO_FORWARD").is_some() {
        return None;
    }
    let mut url = None;
    let mut rest = Vec::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if let Some(u) = a.strip_prefix("--via-mcp=") {
            url = Some(u.to_string());
        } else if a == "--via-mcp" {
            url = it.next().cloned();
        } else {
            rest.push(a.clone());
        }
    }
    // Only the DB-backed groups route to the shared store. `dialogue` has its own
    // endpoint + streaming (watch/poll), and `db-path` is an inherently-local
    // query, so neither is ever forwarded — which also keeps the dialogue monitor
    // pointed at its own MCP regardless of the config-default.
    let group = rest
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str);
    if !matches!(group, Some("memory" | "task" | "metrics")) {
        return None;
    }
    let url = url
        .or_else(|| {
            std::env::var("AGENT_MCP_URL")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .or_else(default_mcp_url_from_config)?;
    Some((url, rest))
}

/// The config-default endpoint: `[agent] mcp_url` in `~/.config/agent/config.toml`.
/// Reuses the same hand-rolled reader as the embed/adjudicate config fallbacks,
/// so there's one config-resolution path for the whole crate.
fn default_mcp_url_from_config() -> Option<String> {
    crate::embed::read_config_toml_value("agent", "mcp_url").filter(|s| !s.is_empty())
}

/// Outcome of a forward attempt. `Done` = the call completed (its result, or a
/// real remote error, was already handled). `FallBackLocal` = the endpoint was
/// unreachable for a READ-only verb, so the caller should serve it from the
/// LOCAL DB. A write to an unreachable endpoint is a hard error (`Err`), never a
/// silent local write — that silent write is exactly the drift this routing kills.
pub enum Outcome {
    Done,
    FallBackLocal,
}

/// Map argv → tool call, invoke the remote, print the result. Applies the
/// unreachable policy (operator decision, 2026-06-24): a READ against an
/// unreachable endpoint falls back to the local DB; a WRITE fails loud. A
/// *reachable* server error propagates as-is (never masked by a local read).
pub fn run(url: &str, args: &[String]) -> Result<Outcome> {
    let (tool, mut arguments) = map_invocation(args)?;
    inline_memory_body(&tool, &mut arguments, &mut std::io::stdin())?;
    match crate::mcp::call_tool(url, &tool, arguments, None) {
        Ok(text) => {
            println!("{text}");
            Ok(Outcome::Done)
        }
        // The endpoint was reached but the server returned an error — surface it;
        // don't fall back to a (possibly stale) local read.
        Err(e) if endpoint_reachable(url) => Err(e),
        // Endpoint unreachable: writes fail loud, reads fall back to local.
        Err(_) if agent::classify::is_write(&tool) => bail!(
            "agent-mcp at {url} is unreachable; refusing to run write `{tool}` against the local DB \
             (that would re-introduce the per-machine drift this routing exists to kill). Bring the \
             endpoint up, or pass an explicit --via-mcp / set AGENT_MCP_URL to override."
        ),
        Err(_) => {
            log::warn!("agent-mcp at {url} unreachable; serving read `{tool}` from the local DB");
            Ok(Outcome::FallBackLocal)
        }
    }
}

/// Best-effort TCP reachability probe, used ONLY on the error path to tell
/// "endpoint down" from "server reached but returned an error". A parse/connect
/// failure reads as unreachable (→ the read/write policy applies).
fn endpoint_reachable(url: &str) -> bool {
    host_port(url).is_some_and(|addr| {
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(800)).is_ok()
    })
}

/// Resolve `host[:port]` from an `http(s)://host[:port]/...` URL to a SocketAddr.
fn host_port(url: &str) -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    let https = url.starts_with("https://");
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let default_port = if https { 443 } else { 80 };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().unwrap_or(default_port)),
        None => (authority, default_port),
    };
    (host, port).to_socket_addrs().ok()?.next()
}

struct ArgMeta {
    name: String,
    bare: bool,
    multiple: bool,
}

/// Descend the live clap tree to the leaf, then assign the trailing tokens to
/// its args by name — the inverse of agent-mcp's `build_argv`.
fn map_invocation(args: &[String]) -> Result<(String, Value)> {
    let root = Cli::command();
    let mut node = &root;
    let mut path: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with('-') {
            break;
        }
        match node.find_subcommand(&args[i]) {
            Some(child) => {
                node = child;
                path.push(args[i].clone());
                i += 1;
            }
            None => break,
        }
    }
    if path.is_empty() {
        bail!("no subcommand to forward (got {args:?})");
    }
    let tool = path.join("_").replace('-', "_");

    // Reuse clap-catalog's walk to get the leaf's arg specs.
    let spec = clap_catalog::command_to_json(node, &path.join(" "));
    let arg_specs = spec
        .get("args")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut positionals: Vec<String> = Vec::new();
    let mut by_long: HashMap<String, ArgMeta> = HashMap::new();
    for a in &arg_specs {
        let name = a
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if a.get("positional")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            positionals.push(name);
        } else if let Some(long) = a.get("long").and_then(Value::as_str) {
            by_long.insert(
                long.to_string(),
                ArgMeta {
                    name,
                    bare: !a
                        .get("takes_value")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    multiple: a.get("multiple").and_then(Value::as_bool).unwrap_or(false),
                },
            );
        }
    }

    let mut arguments = Map::new();
    let mut pos_idx = 0;
    let rest = &args[i..];
    let mut j = 0;
    while j < rest.len() {
        let tok = &rest[j];
        if tok.starts_with("--") {
            // `--flag=value`
            if let Some((flag, val)) = tok.split_once('=') {
                let meta = by_long.get(flag);
                let key = meta
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| flag.trim_start_matches('-').replace('-', "_"));
                put(
                    &mut arguments,
                    &key,
                    meta.is_some_and(|m| m.multiple),
                    Value::String(val.to_string()),
                );
                j += 1;
                continue;
            }
            match by_long.get(tok) {
                Some(meta) if meta.bare => {
                    arguments.insert(meta.name.clone(), Value::Bool(true));
                    j += 1;
                }
                Some(meta) => {
                    let val = rest
                        .get(j + 1)
                        .ok_or_else(|| anyhow!("flag {tok} expects a value"))?;
                    put(
                        &mut arguments,
                        &meta.name,
                        meta.multiple,
                        Value::String(val.clone()),
                    );
                    j += 2;
                }
                None => bail!("unknown flag {tok} for tool {tool}"),
            }
        } else {
            let name = positionals
                .get(pos_idx)
                .ok_or_else(|| anyhow!("unexpected positional argument: {tok}"))?;
            arguments.insert(name.clone(), Value::String(tok.clone()));
            pos_idx += 1;
            j += 1;
        }
    }
    Ok((tool, Value::Object(arguments)))
}

/// Client-side body resolution for forwarded memory writes (at-efx).
/// `--content-file <path>` and `--content -` name CLIENT-side sources; shipped
/// verbatim, the server resolves them against ITS OWN filesystem/stdin — and
/// agent-mcp spawns the inner `agent` via `.output()` (null stdin), so `-`
/// silently reads as "" there. Resolve both HERE, before the call crosses the
/// wire, honoring the CLI precedence (content-file > stdin > inline); only
/// inline `content` is ever forwarded. `stdin` is injected for tests.
fn inline_memory_body(tool: &str, arguments: &mut Value, stdin: &mut dyn Read) -> Result<()> {
    if !matches!(tool, "memory_add" | "memory_update") {
        return Ok(());
    }
    let Some(map) = arguments.as_object_mut() else {
        return Ok(());
    };
    let body = if let Some(path) = map.remove("content_file") {
        let path = path
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("--content-file expects a path"))?;
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading --content-file {path} on the client"))?;
        Some(body)
    } else if map.get("content").and_then(Value::as_str) == Some("-") {
        let mut buf = String::new();
        stdin
            .read_to_string(&mut buf)
            .context("reading content from stdin")?;
        Some(buf)
    } else {
        None
    };
    if let Some(body) = body {
        if body.trim().is_empty() {
            bail!(
                "memory body for `{tool}` resolved to empty/whitespace-only content on the \
                 client; refusing to forward a blank memory"
            );
        }
        map.insert("content".to_string(), Value::String(body));
    }
    Ok(())
}

fn put(arguments: &mut Map<String, Value>, name: &str, multiple: bool, val: Value) {
    if multiple {
        match arguments.get_mut(name) {
            Some(Value::Array(arr)) => arr.push(val),
            _ => {
                arguments.insert(name.to_string(), Value::Array(vec![val]));
            }
        }
    } else {
        arguments.insert(name.to_string(), val);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sv(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn maps_positional_and_value_flag() {
        let (tool, args) =
            map_invocation(&sv(&["memory", "recall", "how do we X", "--k", "6"])).unwrap();
        assert_eq!(tool, "memory_recall");
        assert_eq!(args["query"], "how do we X");
        assert_eq!(args["k"], "6");
    }

    #[test]
    fn maps_bare_flag() {
        // `--no-adjudicate` is a bare SetTrue flag in the real agent (no value).
        let (tool, args) = map_invocation(&sv(&[
            "memory",
            "add",
            "--type",
            "feedback",
            "--name",
            "n",
            "--description",
            "d",
            "--content",
            "c",
            "--no-adjudicate",
        ]))
        .unwrap();
        assert_eq!(tool, "memory_add");
        assert_eq!(args["type"], "feedback");
        assert_eq!(args["no_adjudicate"], true); // bare flag → bool, no value consumed
    }

    #[test]
    fn put_accumulates_multiple_else_overwrites() {
        let mut multi = Map::new();
        put(&mut multi, "tags", true, json!("a"));
        put(&mut multi, "tags", true, json!("b"));
        assert_eq!(multi["tags"], json!(["a", "b"]));

        let mut single = Map::new();
        put(&mut single, "k", false, json!("1"));
        put(&mut single, "k", false, json!("2"));
        assert_eq!(single["k"], json!("2"));
    }

    #[test]
    fn target_extracts_url_and_strips_flag() {
        let (url, rest) = target(&sv(&["--via-mcp", "http://x/mcp", "memory", "list"])).unwrap();
        assert_eq!(url, "http://x/mcp");
        assert_eq!(rest, sv(&["memory", "list"]));
        let (url2, rest2) = target(&sv(&["--via-mcp=http://y/mcp", "task", "list"])).unwrap();
        assert_eq!(url2, "http://y/mcp");
        assert_eq!(rest2, sv(&["task", "list"]));
    }

    /// Completeness ("can't list" guard): every DB-backed leaf the CLI exposes
    /// maps to a non-empty agent-mcp tool name, so routing can never silently
    /// drop a verb — the failure that motivated this work.
    #[test]
    fn every_db_leaf_forwards_to_a_tool() {
        let root = Cli::command();
        let mut leaves: Vec<Vec<String>> = Vec::new();
        collect_leaves(&root, &mut Vec::new(), &mut leaves);
        let db_leaves: Vec<&Vec<String>> = leaves
            .iter()
            .filter(|p| {
                matches!(
                    p.first().map(String::as_str),
                    Some("memory" | "task" | "metrics")
                )
            })
            .collect();
        assert!(!db_leaves.is_empty(), "no DB-backed leaves found");
        for path in db_leaves {
            let (tool, _) =
                map_invocation(path).unwrap_or_else(|e| panic!("leaf {path:?} did not map: {e}"));
            assert!(
                !tool.is_empty(),
                "leaf {path:?} produced an empty tool name"
            );
            // Always classifiable — asserts the wiring; agent-mcp's
            // validate_read_classification is the real drift guard.
            let _ = agent::classify::is_write(&tool);
        }
    }

    fn collect_leaves(cmd: &clap::Command, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        let mut subs = cmd.get_subcommands().peekable();
        if subs.peek().is_none() {
            if !prefix.is_empty() {
                out.push(prefix.clone());
            }
            return;
        }
        for sub in subs {
            prefix.push(sub.get_name().to_string());
            collect_leaves(sub, prefix, out);
            prefix.pop();
        }
    }

    #[test]
    fn dialogue_and_dbpath_are_not_forwarded() {
        // Even with an explicit --via-mcp, the non-DB groups stay local.
        assert!(target(&sv(&["--via-mcp", "http://x/mcp", "dialogue", "peers"])).is_none());
        assert!(target(&sv(&["db-path"])).is_none());
        // DB-backed groups still forward when a url is present.
        assert!(target(&sv(&["--via-mcp", "http://x/mcp", "memory", "list"])).is_some());
    }

    #[test]
    fn host_port_resolves() {
        assert!(host_port("http://127.0.0.1:7700/mcp").is_some());
        assert!(host_port("https://127.0.0.1/mcp").is_some());
        assert!(host_port("not-a-url").is_none());
    }

    // ── at-efx: memory bodies resolve on the CLIENT before forwarding ──

    fn temp_dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("fwd-atefx-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn content_file_is_read_client_side_and_inlined() {
        let dir = temp_dir();
        let path = dir.join("body.md");
        std::fs::write(&path, "the real body\n").unwrap();
        let (tool, mut args) = map_invocation(&sv(&[
            "memory",
            "add",
            "--type",
            "project",
            "--name",
            "n",
            "--description",
            "d",
            "--content-file",
            path.to_str().unwrap(),
        ]))
        .unwrap();
        inline_memory_body(&tool, &mut args, &mut std::io::empty()).unwrap();
        assert_eq!(args["content"], "the real body\n");
        assert!(
            args.get("content_file").is_none(),
            "content_file must never cross the wire"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_dash_reads_client_stdin() {
        let (tool, mut args) =
            map_invocation(&sv(&["memory", "update", "m-1", "--content", "-"])).unwrap();
        inline_memory_body(&tool, &mut args, &mut "piped body".as_bytes()).unwrap();
        assert_eq!(args["content"], "piped body");
    }

    #[test]
    fn missing_or_blank_client_bodies_fail_loud() {
        let dir = temp_dir();
        // Missing file: error on the client, never forwarded for the server to guess at.
        let (tool, mut args) = map_invocation(&sv(&[
            "memory",
            "update",
            "m-1",
            "--content-file",
            dir.join("nope.md").to_str().unwrap(),
        ]))
        .unwrap();
        assert!(inline_memory_body(&tool, &mut args, &mut std::io::empty()).is_err());
        // Whitespace-only file: a "successful" read that would blank the memory.
        let blank = dir.join("blank.md");
        std::fs::write(&blank, "  \n\t\n").unwrap();
        let (tool, mut args) = map_invocation(&sv(&[
            "memory",
            "update",
            "m-1",
            "--content-file",
            blank.to_str().unwrap(),
        ]))
        .unwrap();
        assert!(inline_memory_body(&tool, &mut args, &mut std::io::empty()).is_err());
        // Empty stdin on `--content -`: exactly the null-stdin blanking bug.
        let (tool, mut args) = map_invocation(&sv(&["memory", "add", "--content", "-"])).unwrap();
        assert!(inline_memory_body(&tool, &mut args, &mut std::io::empty()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inline_content_and_other_tools_pass_through_untouched() {
        // Plain inline content is forwarded as-is (the server validates blanks).
        let (tool, mut args) =
            map_invocation(&sv(&["memory", "update", "m-1", "--content", "real"])).unwrap();
        inline_memory_body(&tool, &mut args, &mut std::io::empty()).unwrap();
        assert_eq!(args["content"], "real");
        // Non-memory-write tools are never rewritten, even with a "-" value.
        let (tool, mut args) = map_invocation(&sv(&["memory", "recall", "-", "--k", "2"])).unwrap();
        let before = args.clone();
        inline_memory_body(&tool, &mut args, &mut std::io::empty()).unwrap();
        assert_eq!(args, before);
    }
}
