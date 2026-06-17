//! Generate an MCP `inputSchema` (JSON Schema) from a `ToolSpec`.
//!
//! Scalar value-args are typed `string` (the CLI parses strings for every
//! field), which sidesteps int-vs-float default coercion entirely; enums
//! become `enum`, repeatable args become arrays, bare flags become booleans.

use crate::catalog::{ArgSpec, ToolSpec};
use serde_json::{json, Map, Value};

/// Build the `{type:object, properties, required, [oneOf]}` schema.
pub fn input_schema(spec: &ToolSpec) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    let mut content_key: Option<String> = None;
    let mut content_file_key: Option<String> = None;

    for a in &spec.args {
        // `--json` is managed automatically; not user-facing.
        if a.name == "json" {
            continue;
        }
        props.insert(a.name.clone(), prop_schema(a));
        if a.required {
            required.push(Value::String(a.name.clone()));
        }
        match a.name.as_str() {
            "content" => content_key = Some(a.name.clone()),
            "content_file" | "content-file" => content_file_key = Some(a.name.clone()),
            _ => {}
        }
    }

    let mut schema = json!({
        "type": "object",
        "properties": Value::Object(props),
        "required": Value::Array(required),
    });

    // content vs content-file are mutually exclusive (memory add AND update).
    if let (Some(c), Some(cf)) = (content_key, content_file_key) {
        schema.as_object_mut().unwrap().insert(
            "oneOf".into(),
            json!([{ "required": [c] }, { "required": [cf] }]),
        );
    }

    schema
}

fn prop_schema(a: &ArgSpec) -> Value {
    let mut o = Map::new();
    if a.is_bool() {
        o.insert("type".into(), json!("boolean"));
    } else if a.multiple {
        o.insert("type".into(), json!("array"));
        o.insert("items".into(), json!({ "type": "string" }));
    } else {
        o.insert("type".into(), json!("string"));
        if !a.possible_values.is_empty() {
            o.insert("enum".into(), json!(a.possible_values));
        }
    }
    if let Some(h) = a.help.as_ref().or(a.value_name.as_ref()) {
        o.insert("description".into(), json!(h));
    }
    if let Some(d) = &a.default {
        if a.is_bool() {
            o.insert("default".into(), json!(d == "true"));
        } else {
            o.insert("default".into(), json!(d));
        }
    }
    Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::parse_catalog;
    use serde_json::json;

    fn specs() -> Vec<ToolSpec> {
        let root = json!({
            "name": "agent", "path": "agent", "invokable": false, "subcommands": [
                { "name": "memory", "path": "agent memory", "invokable": false, "subcommands": [
                    { "name": "add", "path": "agent memory add", "about": "Add", "invokable": true, "args": [
                        { "name": "type", "long": "--type", "required": true, "takes_value": true, "possible_values": ["user","feedback"] },
                        { "name": "content", "long": "--content", "required": false, "takes_value": true },
                        { "name": "content_file", "long": "--content-file", "required": false, "takes_value": true },
                        { "name": "tags", "long": "--tags", "required": false, "takes_value": true, "multiple": true }
                    ]},
                    { "name": "recall", "path": "agent memory recall", "about": "Recall", "invokable": true, "args": [
                        { "name": "query", "long": null, "positional": true, "required": true, "takes_value": true },
                        { "name": "k", "long": "--k", "required": false, "takes_value": true, "default": ["5"] }
                    ]}
                ]},
                { "name": "memory_recall_read", "path": "agent memory recall", "invokable": false, "subcommands": [] }
            ]
        });
        // de-dupe the stray non-leaf above by re-parsing only real leaves
        parse_catalog(&json!({
            "name": "agent", "path": "agent", "invokable": false, "subcommands":
                root["subcommands"].as_array().unwrap().clone()
        }))
        .unwrap()
    }

    #[test]
    fn recall_schema_requires_query_and_defaults_k() {
        let specs = specs();
        let recall = specs.iter().find(|s| s.name == "memory_recall").unwrap();
        let sch = input_schema(recall);
        assert_eq!(sch["required"], json!(["query"]));
        assert_eq!(sch["properties"]["query"]["type"], "string");
        assert_eq!(sch["properties"]["k"]["type"], "string");
        assert_eq!(sch["properties"]["k"]["default"], "5");
    }

    #[test]
    fn add_schema_enum_array_and_oneof() {
        let specs = specs();
        let add = specs.iter().find(|s| s.name == "memory_add").unwrap();
        let sch = input_schema(add);
        assert_eq!(
            sch["properties"]["type"]["enum"],
            json!(["user", "feedback"])
        );
        assert_eq!(sch["properties"]["tags"]["type"], "array");
        assert!(sch.get("oneOf").is_some(), "content/content_file -> oneOf");
    }
}
