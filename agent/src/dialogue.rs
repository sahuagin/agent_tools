//! `agent dialogue` — talk to a mu-dialogue MCP server (the inter-agent
//! mailbox), like any external MCP client. `watch` is built to run under a
//! Monitor: it long-polls and prints ONE compact line per NEW message, so an
//! idle agent is woken only when a peer actually writes. `poll`/`say`/`peers`
//! are the one-shot equivalents. Uses the shared sync `mcp` client (no async).

use std::{thread, time::Duration};

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{json, Value};

use crate::mcp;

/// Default endpoint when nothing else is configured. Resolution order:
/// `--url` flag → `AGENT_DIALOGUE_URL` env → `[dialogue].url` in
/// ~/.config/agent/config.toml → this localhost fallback. Deployments point at
/// their real server via config/env, so no host-specific address is baked in.
const DEFAULT_URL: &str = "http://localhost:7740/mcp";

#[derive(Args)]
pub struct DialogueCmd {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Long-poll for new messages to <id>; print one line per message (for Monitor).
    Watch(Watch),
    /// One-shot poll; print the raw {"messages":[...]} JSON.
    Poll(Poll),
    /// Send a message to a peer.
    Say(Say),
    /// List peers currently on the channel (raw JSON).
    Peers(Peers),
}

#[derive(Args)]
struct Watch {
    /// Recipient peer id to watch, e.g. cc:<session-id>.
    id: String,
    /// Only surface messages with ts greater than this (epoch ms). 0 = all backlog.
    #[arg(long, default_value_t = 0)]
    since: i64,
    /// Server-side long-poll wait per cycle (ms).
    #[arg(long, default_value_t = 25000)]
    timeout_ms: u64,
    /// MCP endpoint (default: AGENT_DIALOGUE_URL or the deployed server).
    #[arg(long)]
    url: Option<String>,
}

#[derive(Args)]
struct Poll {
    /// Recipient peer id to poll for.
    id: String,
    #[arg(long, default_value_t = 0)]
    since: i64,
    #[arg(long, default_value_t = 0)]
    timeout_ms: u64,
    #[arg(long)]
    url: Option<String>,
}

#[derive(Args)]
struct Say {
    #[arg(long)]
    from: String,
    #[arg(long)]
    to: String,
    #[arg(long)]
    content: String,
    /// Optional thread id to group a multi-turn conversation.
    #[arg(long)]
    thread: Option<String>,
    #[arg(long)]
    url: Option<String>,
}

#[derive(Args)]
struct Peers {
    /// Filter to one role (e.g. cc, mu).
    #[arg(long)]
    role: Option<String>,
    #[arg(long)]
    url: Option<String>,
}

fn endpoint(url: &Option<String>) -> String {
    url.clone()
        .or_else(|| crate::embed::resolve_setting("dialogue", "url", "AGENT_DIALOGUE_URL"))
        .unwrap_or_else(|| DEFAULT_URL.to_string())
}

pub fn run(cmd: DialogueCmd) -> Result<()> {
    match cmd.action {
        Action::Watch(w) => watch(w),
        Action::Poll(p) => {
            let text = mcp::call_tool(
                &endpoint(&p.url),
                "dialogue_poll",
                json!({"to": p.id, "since": p.since, "timeout_ms": p.timeout_ms}),
                Some(Duration::from_millis(p.timeout_ms + 10_000)),
            )?;
            println!("{text}");
            Ok(())
        }
        Action::Say(s) => {
            let mut args = json!({"from": s.from, "to": s.to, "content": s.content});
            if let Some(t) = &s.thread {
                args["session_thread"] = json!(t);
            }
            let text = mcp::call_tool(&endpoint(&s.url), "dialogue_say", args, None)?;
            println!("{text}");
            Ok(())
        }
        Action::Peers(p) => {
            let mut args = json!({});
            if let Some(r) = &p.role {
                args["role"] = json!(r);
            }
            let text = mcp::call_tool(&endpoint(&p.url), "dialogue_peers", args, None)?;
            println!("{text}");
            Ok(())
        }
    }
}

/// Long-poll loop: each NEW message → one `DIALOGUE <from>: <snippet>` line
/// (one Monitor notification). Read full content via `dialogue poll` when
/// alerted. Resilient to transient errors (logs + retries); runs until killed.
fn watch(w: Watch) -> Result<()> {
    let url = endpoint(&w.url);
    let read_timeout = Some(Duration::from_millis(w.timeout_ms + 10_000));
    let mut since = w.since;
    loop {
        let text = match mcp::call_tool(
            &url,
            "dialogue_poll",
            json!({"to": w.id, "since": since, "timeout_ms": w.timeout_ms}),
            read_timeout,
        ) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("dialogue watch poll failed: {e}");
                thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
        // call_tool returns the tool's text block, which IS the
        // {"messages":[...]} JSON the dialogue server emits.
        let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(msgs) = parsed.get("messages").and_then(Value::as_array) else {
            continue;
        };
        let mut emitted = false;
        for m in msgs {
            let from = m.get("from").and_then(Value::as_str).unwrap_or("?");
            let content = m.get("content").and_then(Value::as_str).unwrap_or("");
            let snippet: String = content
                .chars()
                .take(180)
                .collect::<String>()
                .replace('\n', " ");
            println!("DIALOGUE {from}: {snippet}");
            emitted = true;
            if let Some(ts) = m.get("ts").and_then(Value::as_i64) {
                if ts > since {
                    since = ts;
                }
            }
        }
        if emitted {
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    }
}
