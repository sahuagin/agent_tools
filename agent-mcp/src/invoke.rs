//! Turn an MCP tool call into an `agent` subprocess invocation.
//!
//! Exec-array only (no shell), so quoting hazards in values like `--content`
//! are moot. `agent` writes results to stdout and diagnostics to stderr.

use crate::catalog::ToolSpec;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Map, Value};

/// Build the argv (after the `agent` binary) for a tool call.
///
/// Order: subcommand path, then positionals (bare, in declared order), then
/// options. `long` is already `--`-prefixed (emitted verbatim). Bare flags
/// emit only when true; repeatable args repeat the flag; `--json` is appended
/// automatically for leaves that support it.
pub fn build_argv(spec: &ToolSpec, args: &Map<String, Value>) -> Result<Vec<String>> {
    let mut out = spec.argv.clone();

    for a in spec.args.iter().filter(|a| a.positional) {
        match args.get(&a.name) {
            Some(v) => out.push(scalar_to_string(v)?),
            None if a.required => bail!("missing required positional '{}'", a.name),
            None => {}
        }
    }

    for a in spec.args.iter().filter(|a| !a.positional) {
        if a.name == "json" {
            continue;
        }
        let Some(v) = args.get(&a.name) else { continue };
        let long = a
            .long
            .clone()
            .ok_or_else(|| anyhow!("non-positional arg '{}' has no long flag", a.name))?;

        if a.is_bare_flag() {
            // clap SetTrue: emit the flag only when explicitly true.
            if v.as_bool() == Some(true) {
                out.push(long);
            }
        } else if a.multiple {
            let arr = v
                .as_array()
                .ok_or_else(|| anyhow!("arg '{}' expects an array", a.name))?;
            for el in arr {
                out.push(long.clone());
                out.push(scalar_to_string(el)?);
            }
        } else {
            out.push(long);
            out.push(scalar_to_string(v)?);
        }
    }

    if spec.has_json_flag {
        out.push("--json".into());
    }
    Ok(out)
}

/// Stringify a scalar JSON value for the command line. Numbers and bools are
/// rendered (an MCP client may send a JSON number for a string-typed field).
fn scalar_to_string(v: &Value) -> Result<String> {
    Ok(match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => bail!("expected a scalar value, got {other}"),
    })
}

/// Result of running `agent`: (success, stdout, stderr).
pub fn run_agent(agent_bin: &str, argv: &[String]) -> Result<(bool, String, String)> {
    let out = std::process::Command::new(agent_bin)
        .args(argv)
        .output()
        .with_context(|| format!("spawning `{agent_bin}`"))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::parse_catalog;
    use serde_json::json;

    fn spec(name: &str) -> ToolSpec {
        let root = json!({
            "name": "agent", "path": "agent", "invokable": false, "subcommands": [
                { "name": "memory", "path": "agent memory", "invokable": false, "subcommands": [
                    { "name": "add", "path": "agent memory add", "about": "Add", "invokable": true, "args": [
                        { "name": "type", "long": "--type", "required": true, "takes_value": true },
                        { "name": "tags", "long": "--tags", "required": false, "takes_value": true, "multiple": true },
                        { "name": "no-adjudicate", "long": "--no-adjudicate", "required": false, "takes_value": false, "possible_values": ["true","false"] }
                    ]},
                    { "name": "recall", "path": "agent memory recall", "about": "Recall", "invokable": true, "args": [
                        { "name": "query", "long": null, "positional": true, "required": true, "takes_value": true },
                        { "name": "json", "long": "--json", "required": false, "takes_value": false, "possible_values": ["true","false"] }
                    ]}
                ]}
            ]
        });
        parse_catalog(&root)
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap()
    }

    #[test]
    fn positional_first_and_json_appended() {
        let argv = build_argv(
            &spec("memory_recall"),
            json!({ "query": "how do we X" }).as_object().unwrap(),
        )
        .unwrap();
        assert_eq!(argv, vec!["memory", "recall", "how do we X", "--json"]);
    }

    #[test]
    fn long_emitted_verbatim_no_double_dash() {
        let argv = build_argv(
            &spec("memory_add"),
            json!({ "type": "feedback" }).as_object().unwrap(),
        )
        .unwrap();
        assert_eq!(argv, vec!["memory", "add", "--type", "feedback"]);
        assert!(!argv.iter().any(|a| a.starts_with("----")));
    }

    #[test]
    fn bare_flag_only_when_true_and_multiple_repeats() {
        let argv = build_argv(
            &spec("memory_add"),
            json!({ "type": "user", "tags": ["a", "b"], "no-adjudicate": true })
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert!(argv.contains(&"--no-adjudicate".to_string()));
        // tags repeated
        assert_eq!(argv.iter().filter(|a| *a == "--tags").count(), 2);

        let argv2 = build_argv(
            &spec("memory_add"),
            json!({ "type": "user", "no-adjudicate": false })
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert!(!argv2.contains(&"--no-adjudicate".to_string()));
    }
}
