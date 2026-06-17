//! Build the tool catalog by introspecting `agent --help-ai --json`.
//!
//! The catalog walks the live clap tree, so the MCP tool surface cannot
//! drift from the actual CLI. Each `invokable` leaf becomes one `ToolSpec`.

use anyhow::{bail, Result};
use serde_json::Value;

/// One CLI argument as surfaced by `agent --help-ai --json`.
#[derive(Debug, Clone)]
pub struct ArgSpec {
    /// clap id — the JSON property key (e.g. `content_file`, hyphen-free).
    pub name: String,
    /// Long flag, **already `--`-prefixed** in the catalog (main.rs:179).
    pub long: Option<String>,
    pub positional: bool,
    pub required: bool,
    /// `false` => a bare boolean flag (clap `SetTrue`).
    pub takes_value: bool,
    pub multiple: bool,
    pub help: Option<String>,
    pub value_name: Option<String>,
    pub possible_values: Vec<String>,
    /// Default value as clap emits it (always a string).
    pub default: Option<String>,
}

impl ArgSpec {
    /// A boolean-valued arg: either a bare `SetTrue` flag (`takes_value==false`)
    /// or a value-typed `Option<bool>` (`possible_values == [true,false]`).
    pub fn is_bool(&self) -> bool {
        self.is_bare_flag()
            || (self.possible_values.len() == 2
                && self.possible_values.iter().any(|v| v == "true")
                && self.possible_values.iter().any(|v| v == "false"))
    }

    /// Bare flag emitted with no value when true (clap `SetTrue`); distinct
    /// from a value-typed bool which needs `--flag <value>`.
    pub fn is_bare_flag(&self) -> bool {
        !self.takes_value
    }
}

/// One invokable CLI leaf, mapped to an MCP tool.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    /// snake_case, no leading `agent` segment (e.g. `memory_add`).
    pub name: String,
    /// Subcommand path minus the binary (e.g. `["memory", "add"]`).
    pub argv: Vec<String>,
    /// Description = the leaf's `about` (per-leaf `help_ai` does not exist in
    /// a single full-tree dump — it lives only on the root node).
    pub description: String,
    pub args: Vec<ArgSpec>,
    pub is_write: bool,
    /// The leaf has a `--json` flag, so machine output can be requested.
    pub has_json_flag: bool,
}

/// Leaf names (normalized snake_case) that ONLY read. Everything else is
/// treated as a write and gated behind `--allow-writes`. A startup assertion
/// checks every entry maps to a real leaf, so this cannot silently drift.
const READ_TOOLS: &[&str] = &[
    "memory_show",
    "memory_events",
    "memory_patch_log",
    "memory_diff",
    "memory_search",
    "memory_recent",
    "memory_list",
    "memory_context",
    "memory_context_stats",
    "memory_recall",
    "memory_recall_stats",
    "memory_resolve",
    "memory_kernel_show",
    "memory_export",
    "task_list",
    "task_show",
    "task_resume",
    "metrics_report",
    "metrics_list",
    "db_path",
];

/// Parse the full `agent --help-ai --json` tree into one `ToolSpec` per
/// invokable leaf, with collision + drift guards.
pub fn parse_catalog(root: &Value) -> Result<Vec<ToolSpec>> {
    let mut specs = Vec::new();
    collect(root, &mut specs);

    if specs.is_empty() {
        bail!("no invokable tools found in catalog");
    }

    // Name-collision guard: rmcp dispatches by name string, so duplicates
    // after snake_case normalization would silently shadow each other.
    let mut seen = std::collections::HashSet::new();
    for s in &specs {
        if !seen.insert(s.name.as_str()) {
            bail!("duplicate tool name after normalization: {}", s.name);
        }
    }

    Ok(specs)
}

/// Startup invariant: every `READ_TOOLS` entry must correspond to a live leaf,
/// so the hand-maintained read allow-list cannot silently drift from the CLI.
/// Kept separate from `parse_catalog` so unit tests can use partial fixtures.
pub fn validate_read_classification(specs: &[ToolSpec]) -> Result<()> {
    for r in READ_TOOLS {
        if !specs.iter().any(|s| s.name == *r) {
            bail!("READ_TOOLS entry '{r}' is not a live tool (catalog drift)");
        }
    }
    Ok(())
}

fn collect(node: &Value, out: &mut Vec<ToolSpec>) {
    if node
        .get("invokable")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        if let Some(spec) = tool_from_node(node) {
            out.push(spec);
        }
    }
    if let Some(subs) = node.get("subcommands").and_then(Value::as_array) {
        for c in subs {
            collect(c, out);
        }
    }
}

fn tool_from_node(node: &Value) -> Option<ToolSpec> {
    let path = node.get("path")?.as_str()?;
    // Drop the leading `agent` segment; the rest is the subcommand path.
    let rest = path.strip_prefix("agent ").unwrap_or(path);
    if rest.is_empty() || rest == "agent" {
        return None; // the root node itself (not invokable anyway)
    }
    let argv: Vec<String> = rest.split_whitespace().map(String::from).collect();
    let name = rest.replace([' ', '-'], "_");
    let description = node
        .get("about")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let args: Vec<ArgSpec> = node
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(arg_from_json).collect())
        .unwrap_or_default();
    let has_json_flag = args.iter().any(|a| a.name == "json");
    let is_write = !READ_TOOLS.contains(&name.as_str());
    Some(ToolSpec {
        name,
        argv,
        description,
        args,
        is_write,
        has_json_flag,
    })
}

fn arg_from_json(v: &Value) -> ArgSpec {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    let possible_values = v
        .get("possible_values")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    // `default` is emitted as an array of strings; take the first.
    let default = v
        .get("default")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(String::from);
    ArgSpec {
        name: s("name").unwrap_or_default(),
        long: s("long"),
        positional: b("positional"),
        required: b("required"),
        takes_value: b("takes_value"),
        multiple: b("multiple"),
        help: s("help"),
        value_name: s("value_name"),
        possible_values,
        default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        json!({
            "name": "agent", "path": "agent", "invokable": false,
            "subcommands": [
                { "name": "memory", "path": "agent memory", "invokable": false, "subcommands": [
                    { "name": "add", "path": "agent memory add", "about": "Add a new memory", "invokable": true, "args": [
                        { "name": "type", "long": "--type", "positional": false, "required": true, "takes_value": true, "multiple": false },
                        { "name": "content", "long": "--content", "positional": false, "required": false, "takes_value": true, "multiple": false },
                        { "name": "content_file", "long": "--content-file", "positional": false, "required": false, "takes_value": true, "multiple": false },
                        { "name": "tags", "long": "--tags", "positional": false, "required": false, "takes_value": true, "multiple": true },
                        { "name": "no-adjudicate", "long": "--no-adjudicate", "positional": false, "required": false, "takes_value": false, "multiple": false, "possible_values": ["true","false"] }
                    ]},
                    { "name": "recall", "path": "agent memory recall", "about": "Semantic recall", "invokable": true, "args": [
                        { "name": "query", "long": null, "positional": true, "required": true, "takes_value": true, "multiple": false },
                        { "name": "k", "long": "--k", "positional": false, "required": false, "takes_value": true, "multiple": false, "default": ["5"] },
                        { "name": "json", "long": "--json", "positional": false, "required": false, "takes_value": false, "multiple": false, "possible_values": ["true","false"] }
                    ]}
                ]},
                { "name": "db-path", "path": "agent db-path", "about": "Print path", "invokable": true, "args": [] }
            ]
        })
    }

    #[test]
    fn parses_invokable_leaves() {
        let specs = parse_catalog(&fixture()).unwrap();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"memory_add"));
        assert!(names.contains(&"memory_recall"));
        assert!(names.contains(&"db_path"));
        // groups are not leaves
        assert!(!names.contains(&"memory"));
    }

    #[test]
    fn classifies_read_vs_write() {
        let specs = parse_catalog(&fixture()).unwrap();
        let g = |n: &str| specs.iter().find(|s| s.name == n).unwrap();
        assert!(g("memory_add").is_write);
        assert!(!g("memory_recall").is_write);
        assert!(!g("db_path").is_write);
    }

    #[test]
    fn argv_and_json_flag() {
        let specs = parse_catalog(&fixture()).unwrap();
        let recall = specs.iter().find(|s| s.name == "memory_recall").unwrap();
        assert_eq!(recall.argv, vec!["memory", "recall"]);
        assert!(recall.has_json_flag);
        let add = specs.iter().find(|s| s.name == "memory_add").unwrap();
        assert!(!add.has_json_flag);
        // bare flag vs value-typed bool
        let no_adj = add.args.iter().find(|a| a.name == "no-adjudicate").unwrap();
        assert!(no_adj.is_bare_flag());
        assert!(no_adj.is_bool());
    }
}
