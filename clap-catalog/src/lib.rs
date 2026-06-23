//! `clap-catalog` — emit a structured JSON catalog of a clap `Command` tree.
//!
//! **Autocomplete for machines.** This is the same shape as `clap_complete`
//! (walk the live `clap::Command` and emit a machine-readable description of
//! the CLI surface) — but the consumer is an LLM agent / MCP server rather than
//! a shell. Because it walks the real command tree, the catalog can never drift
//! from the actual CLI.
//!
//! Each `invokable` leaf (a subcommand with no children) is the unit a tool
//! layer maps to a callable tool. Every arg carries enough metadata —
//! `long`/`short`/`positional`/`required`/`takes_value`/`multiple`/
//! `possible_values`/`default`/`help` — to build an invocation or a JSON-Schema
//! input schema.
//!
//! Instrument any clap program with a few lines (before `Cli::parse()`):
//! ```ignore
//! let argv: Vec<String> = std::env::args().skip(1).collect();
//! if argv.iter().any(|a| a == "--help-ai") {
//!     let path: Vec<&str> = argv.iter()
//!         .map(String::as_str)
//!         .filter(|s| !s.starts_with('-'))
//!         .collect();
//!     let spec = clap_catalog::catalog_scoped::<Cli>(&path);
//!     println!("{}", serde_json::to_string_pretty(&spec).unwrap());
//!     return Ok(());
//! }
//! ```

use clap::{Arg, ArgAction, Command, CommandFactory};
use serde_json::{json, Map, Value};

/// Full catalog for a `#[derive(Parser)]` type, rooted at its command name.
pub fn catalog<C: CommandFactory>() -> Value {
    let cmd = C::command();
    let name = cmd.get_name().to_string();
    command_to_json(&cmd, &name)
}

/// Catalog scoped to a subcommand path (e.g. `["memory", "add"]`). Descends
/// from the root following each segment; an unknown segment stops the descent
/// and the deepest node reached is returned. Empty segments are skipped, so it
/// is safe to pass the raw positional tokens of a command line.
pub fn catalog_scoped<C: CommandFactory>(path: &[&str]) -> Value {
    let root = C::command();
    let mut node = &root;
    let mut full = vec![root.get_name().to_string()];
    for seg in path.iter().copied().filter(|s| !s.is_empty()) {
        match node.find_subcommand(seg) {
            Some(child) => {
                node = child;
                full.push(seg.to_string());
            }
            None => break,
        }
    }
    command_to_json(node, &full.join(" "))
}

/// Recursively render a clap `Command` as the `--help-ai` superset
/// (crates/t4c/docs/help-ai-standard.md): `name`, discovery-facing `summary`
/// (from clap's `about`; also emitted as `about` for back-compat), full
/// invocation `path`, `aliases`, `invokable` (true for leaves — the nodes a tool
/// layer maps to a callable tool), rich `args`, and nested `subcommands`.
/// (`keywords` / `output_schema` have no clap source, so they're left to a
/// hand-rolled emitter; unknown fields are forward-compatible regardless.)
pub fn command_to_json(cmd: &Command, path: &str) -> Value {
    let args: Vec<Value> = cmd
        .get_arguments()
        .filter(|a| {
            // Drop clap's auto-generated --help/--version.
            !matches!(
                a.get_action(),
                ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
            )
        })
        .map(arg_to_json)
        .collect();

    let subcommands: Vec<Value> = cmd
        .get_subcommands()
        .filter(|c| c.get_name() != "help") // skip clap's auto `help` subcommand
        .map(|c| command_to_json(c, &format!("{path} {}", c.get_name())))
        .collect();

    let mut obj = Map::new();
    obj.insert("name".into(), json!(cmd.get_name()));
    obj.insert("path".into(), json!(path));
    if let Some(about) = cmd.get_about() {
        let about = about.to_string();
        // The superset standard's discovery-facing field is `summary` (NOT clap's
        // native `about`). Emit `summary` as the canonical name; keep `about` too
        // so existing clap-catalog consumers don't break.
        obj.insert("summary".into(), json!(about.clone()));
        obj.insert("about".into(), json!(about));
    }
    let aliases: Vec<String> = cmd.get_all_aliases().map(str::to_string).collect();
    if !aliases.is_empty() {
        obj.insert("aliases".into(), json!(aliases));
    }
    obj.insert("invokable".into(), json!(subcommands.is_empty()));
    if !args.is_empty() {
        obj.insert("args".into(), json!(args));
    }
    if !subcommands.is_empty() {
        obj.insert("subcommands".into(), json!(subcommands));
    }
    Value::Object(obj)
}

/// Render a single clap `Arg` as JSON: enough for a caller to build an
/// invocation or an MCP input-schema property (flag vs option, required,
/// repeatable, allowed values, default). `long` is emitted with its `--`
/// prefix; bare flags surface as `takes_value: false`.
pub fn arg_to_json(arg: &Arg) -> Value {
    let takes_value = matches!(arg.get_action(), ArgAction::Set | ArgAction::Append);
    let multiple = matches!(arg.get_action(), ArgAction::Append);

    let mut obj = Map::new();
    obj.insert("name".into(), json!(arg.get_id().as_str()));
    if let Some(long) = arg.get_long() {
        obj.insert("long".into(), json!(format!("--{long}")));
    }
    if let Some(short) = arg.get_short() {
        obj.insert("short".into(), json!(format!("-{short}")));
    }
    obj.insert("positional".into(), json!(arg.is_positional()));
    obj.insert("required".into(), json!(arg.is_required_set()));
    obj.insert("takes_value".into(), json!(takes_value));
    obj.insert("multiple".into(), json!(multiple));
    if let Some(vn) = arg.get_value_names().and_then(<[_]>::first) {
        obj.insert("value_name".into(), json!(vn.to_string()));
    }
    if let Some(help) = arg.get_help() {
        obj.insert("help".into(), json!(help.to_string()));
    }
    let possible: Vec<String> = arg
        .get_possible_values()
        .iter()
        .map(|p| p.get_name().to_string())
        .collect();
    if !possible.is_empty() {
        obj.insert("possible_values".into(), json!(possible));
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    if !defaults.is_empty() {
        obj.insert("default".into(), json!(defaults));
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Args, Parser, Subcommand};

    #[derive(Parser)]
    #[command(name = "demo")]
    struct Cli {
        #[command(subcommand)]
        _cmd: Top,
    }

    #[derive(Subcommand)]
    enum Top {
        /// Memory things
        Memory(MemCmd),
        /// Print the db path
        DbPath,
    }

    #[derive(Args)]
    struct MemCmd {
        #[command(subcommand)]
        _action: MemAction,
    }

    #[derive(Subcommand)]
    enum MemAction {
        /// Add a memory
        Add(AddArgs),
    }

    #[derive(Args)]
    struct AddArgs {
        /// the memory type
        #[arg(long)]
        r#type: String,
        /// repeatable tags
        #[arg(long)]
        tags: Vec<String>,
        /// skip adjudication
        #[arg(long)]
        no_adjudicate: bool,
        /// positional query
        query: Option<String>,
    }

    #[test]
    fn root_lists_groups_and_leaves() {
        let c = catalog::<Cli>();
        assert_eq!(c["name"], "demo");
        assert_eq!(c["invokable"], false);
        let subs = c["subcommands"].as_array().unwrap();
        let names: Vec<&str> = subs.iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"memory"));
        assert!(names.contains(&"db-path"));
        let dbp = subs.iter().find(|s| s["name"] == "db-path").unwrap();
        assert_eq!(dbp["invokable"], true);
    }

    #[test]
    fn scoped_descends_and_args_carry_metadata() {
        let c = catalog_scoped::<Cli>(&["memory", "add"]);
        assert_eq!(c["path"], "demo memory add");
        assert_eq!(c["invokable"], true);
        let args = c["args"].as_array().unwrap();

        let typ = args.iter().find(|a| a["name"] == "type").unwrap();
        assert_eq!(typ["long"], "--type");
        assert_eq!(typ["required"], true);
        assert_eq!(typ["takes_value"], true);

        let tags = args.iter().find(|a| a["name"] == "tags").unwrap();
        assert_eq!(tags["multiple"], true);

        // bare bool flag: takes_value == false, long is hyphenated
        let na = args.iter().find(|a| a["name"] == "no_adjudicate").unwrap();
        assert_eq!(na["long"], "--no-adjudicate");
        assert_eq!(na["takes_value"], false);

        // positional
        let q = args.iter().find(|a| a["name"] == "query").unwrap();
        assert_eq!(q["positional"], true);
    }

    #[test]
    fn summary_aliases_about() {
        // The superset standard's discovery-facing field is `summary`;
        // clap-catalog emits it from clap's `about` and keeps `about` for
        // back-compat (the two carry the same value).
        let c = catalog::<Cli>();
        let mem = c["subcommands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "memory")
            .unwrap();
        assert_eq!(mem["summary"], "Memory things");
        assert_eq!(mem["about"], "Memory things");
        // a nested leaf carries its own summary
        let add = catalog_scoped::<Cli>(&["memory", "add"]);
        assert_eq!(add["summary"], "Add a memory");
    }

    #[test]
    fn unknown_segment_stops_descent() {
        let c = catalog_scoped::<Cli>(&["memory", "nope"]);
        assert_eq!(c["path"], "demo memory");
    }

    #[test]
    fn empty_path_returns_root() {
        let c = catalog_scoped::<Cli>(&[]);
        assert_eq!(c["path"], "demo");
    }
}
