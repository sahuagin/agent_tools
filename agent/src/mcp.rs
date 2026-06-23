//! Minimal synchronous MCP-over-streamable-HTTP client (ureq) — shared by
//! `forward` (argv → tools/call) and `dialogue` (poll/say/peers/watch). The
//! crate deliberately stays sync: no async runtime is pulled into `agent`.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

const ACCEPT: &str = "application/json, text/event-stream";

/// initialize (capturing the session id) → notifications/initialized →
/// tools/call → the result's first text block.
///
/// `read_timeout` bounds the wait on the `tools/call` response; set it ABOVE a
/// long-poll `timeout_ms` so a blocking poll isn't cut short. `None` uses
/// ureq's default.
pub fn call_tool(
    url: &str,
    tool: &str,
    arguments: Value,
    read_timeout: Option<Duration>,
) -> Result<String> {
    let init = json!({
        "jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-03-26","capabilities":{},
            "clientInfo":{"name":"agent-cli","version":env!("CARGO_PKG_VERSION")}
        }
    });
    let resp = ureq::post(url)
        .set("Content-Type", "application/json")
        .set("Accept", ACCEPT)
        .send_json(init)
        .map_err(|e| anyhow!("initialize failed: {e}"))?;
    let session = resp.header("mcp-session-id").map(String::from);
    read_sse_result(resp)?; // drain the initialize result

    let mut notif = ureq::post(url)
        .set("Content-Type", "application/json")
        .set("Accept", ACCEPT);
    if let Some(s) = &session {
        notif = notif.set("Mcp-Session-Id", s);
    }
    let _ = notif.send_json(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));

    let mut call = ureq::post(url)
        .set("Content-Type", "application/json")
        .set("Accept", ACCEPT);
    if let Some(s) = &session {
        call = call.set("Mcp-Session-Id", s);
    }
    if let Some(t) = read_timeout {
        call = call.timeout(t);
    }
    let resp = call
        .send_json(json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":tool,"arguments":arguments}
        }))
        .map_err(|e| anyhow!("tools/call failed: {e}"))?;
    let result = read_sse_result(resp)?;

    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!("{text}");
    }
    Ok(text)
}

/// Extract the JSON-RPC `result` from a streamable-HTTP response (SSE `data:`
/// frames, or a plain JSON body).
fn read_sse_result(resp: ureq::Response) -> Result<Value> {
    let body = resp.into_string().context("reading response body")?;
    let payload = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:").map(str::trim))
        .next_back()
        .or_else(|| body.trim_start().starts_with('{').then(|| body.trim()))
        .ok_or_else(|| anyhow!("no JSON-RPC payload in response: {body:?}"))?;
    let env: Value = serde_json::from_str(payload).context("parsing JSON-RPC envelope")?;
    if let Some(err) = env.get("error") {
        bail!("remote error: {err}");
    }
    env.get("result")
        .cloned()
        .ok_or_else(|| anyhow!("response has no result"))
}
