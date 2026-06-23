//! `--via-mcp <url>` / `AGENT_MCP_URL`: forward an `agent` invocation to a
//! remote agent-mcp as an MCP `tools/call`, instead of opening the local DB.
//!
//! This is the inverse of agent-mcp's argv builder: `["memory","recall","x",
//! "--k","6"]` → tool `memory_recall`, arguments `{"query":"x","k":"6"}`. It
//! uses a synchronous `ureq` client — no async runtime is pulled into `agent`.
//! Loop-safe: forwarding is opt-in, and the server-side `agent` runs normally
//! (local DB), so `agent(client) → agent-mcp → agent(server)` terminates.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use clap::CommandFactory;
use serde_json::{Map, Value};

use crate::Cli;

/// Pull `--via-mcp <url>` / `--via-mcp=<url>` out of argv, returning the URL
/// and the remaining (subcommand) argv. Falls back to `AGENT_MCP_URL`.
pub fn target(argv: &[String]) -> Option<(String, Vec<String>)> {
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
    let url = url.or_else(|| std::env::var("AGENT_MCP_URL").ok())?;
    Some((url, rest))
}

/// Map argv → tool call, invoke the remote, print the result text.
pub fn run(url: &str, args: &[String]) -> Result<()> {
    let (tool, arguments) = map_invocation(args)?;
    let text = crate::mcp::call_tool(url, &tool, arguments, None)?;
    println!("{text}");
    Ok(())
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
}
