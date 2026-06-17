//! In-crate integration test: exercise the full
//! `catalog → registry → schema/gate → invoke → subprocess` path against a
//! stub `agent` binary. Hermetic (no real DB, no installed `agent`); the
//! streamable-HTTP transport itself is the verbatim `code-index-mcp` pattern
//! and is covered by manual smoke, so this focuses on the crate's own logic.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::server::Registry;
use crate::{catalog, invoke};

/// Write a minimal stub `agent` to the test binary's own directory (which is
/// executable — unlike /tmp, which is `noexec` on FreeBSD). It emits a tiny
/// `--help-ai --json` catalog and services `memory add` / `memory list`
/// against `$AGENT_DB`.
fn write_stub_agent() -> PathBuf {
    let dir = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let path = dir.join(format!("stub-agent-{}", std::process::id()));
    let script = r#"#!/bin/sh
help_ai=0; jsonf=0
for a in "$@"; do
  [ "$a" = "--help-ai" ] && help_ai=1
  [ "$a" = "--json" ] && jsonf=1
done
if [ "$help_ai" = 1 ]; then
  if [ "$jsonf" = 1 ]; then
cat <<'JSON'
{"name":"agent","path":"agent","invokable":false,"subcommands":[
 {"name":"memory","path":"agent memory","invokable":false,"subcommands":[
  {"name":"add","path":"agent memory add","about":"add a memory","invokable":true,"args":[
   {"name":"type","long":"--type","positional":false,"required":true,"takes_value":true,"multiple":false},
   {"name":"content","long":"--content","positional":false,"required":true,"takes_value":true,"multiple":false}]},
  {"name":"list","path":"agent memory list","about":"list memories","invokable":true,"args":[]}
 ]}
]}
JSON
  fi
  exit 0
fi
case "$1 $2" in
  "memory add")
    content=""
    while [ $# -gt 0 ]; do [ "$1" = "--content" ] && content="$2"; shift; done
    echo "stub-id-1"
    echo "stub-id-1 $content" >> "$AGENT_DB" ;;
  "memory list")
    cat "$AGENT_DB" 2>/dev/null || true ;;
  *)
    echo "stub: unknown command: $*" >&2; exit 1 ;;
esac
"#;
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(script.as_bytes()).unwrap();
    let mut perms = f.metadata().unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

#[test]
fn registry_gate_and_invoke_round_trip() {
    let stub = write_stub_agent();
    let db = std::env::temp_dir().join(format!("agent-mcp-it-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    // SAFETY: single-threaded w.r.t. AGENT_DB — no other test reads/sets it.
    std::env::set_var("AGENT_DB", &db);

    // catalog → registry (parse_catalog, not the production drift check, since
    // the stub deliberately exposes a tiny surface).
    let out = std::process::Command::new(&stub)
        .args(["--help-ai", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let root: Value = serde_json::from_slice(&out.stdout).unwrap();
    let reg = Registry::new(catalog::parse_catalog(&root).unwrap());

    // write-gate: memory_add (write) is hidden when writes are disabled and
    // present when enabled; memory_list (read) is always listed.
    let gated: Vec<String> = reg
        .tools(false)
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    let open: Vec<String> = reg.tools(true).iter().map(|t| t.name.to_string()).collect();
    assert!(
        !gated.contains(&"memory_add".to_string()),
        "write tool hidden when gated"
    );
    assert!(
        gated.contains(&"memory_list".to_string()),
        "read tool always listed"
    );
    assert!(
        open.contains(&"memory_add".to_string()),
        "write tool listed with --allow-writes"
    );

    // invoke write → read round-trip through build_argv + a real subprocess.
    let add = reg.get("memory_add").unwrap();
    let add_args: Map<String, Value> = json!({ "type": "feedback", "content": "hello-mcp" })
        .as_object()
        .unwrap()
        .clone();
    let argv = invoke::build_argv(add, &add_args).unwrap();
    assert_eq!(
        argv,
        vec![
            "memory",
            "add",
            "--type",
            "feedback",
            "--content",
            "hello-mcp"
        ]
    );
    let (ok, idout, _) = invoke::run_agent(stub.to_str().unwrap(), &argv).unwrap();
    assert!(ok, "memory_add exits zero");
    assert!(idout.contains("stub-id-1"), "stdout carries the new id");

    let list = reg.get("memory_list").unwrap();
    let argv2 = invoke::build_argv(list, &Map::new()).unwrap();
    let (ok2, listout, _) = invoke::run_agent(stub.to_str().unwrap(), &argv2).unwrap();
    assert!(ok2, "memory_list exits zero");
    assert!(
        listout.contains("hello-mcp"),
        "round-trip: list shows the added memory"
    );

    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(&stub);
}
