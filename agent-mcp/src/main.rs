//! agent-mcp: an MCP server exposing the `agent` CLI as tools.
//!
//! Tools are discovered at startup by shelling out to `agent --help-ai --json`
//! and walking the catalog — the surface tracks the CLI automatically. Each
//! invokable leaf becomes one MCP tool; mutating tools are hidden and refused
//! unless `--allow-writes` is given.
//!
//! Launch:
//!   agent-mcp                          # stdio (Claude Code spawns as subprocess)
//!   agent-mcp --listen 0.0.0.0:7700    # streamable HTTP
//!   AGENT_MCP_ADDR=0.0.0.0:7700 agent-mcp
//! Env:
//!   AGENT_BIN=/path/to/agent           # default: `agent` on PATH
//!   AGENT_MCP_ALLOW_WRITES=1           # enable mutating tools

mod catalog;
#[cfg(test)]
mod integration_tests;
mod invoke;
mod schema;
mod server;

use std::sync::Arc;

use anyhow::{bail, Context, Result};

use server::{AgentMcpServer, Registry};

fn resolve_agent_bin() -> String {
    std::env::var("AGENT_BIN").unwrap_or_else(|_| "agent".to_string())
}

/// Shell out to `agent --help-ai --json` once and build the tool registry.
/// Fails fast with a clear error if the binary is missing or unparseable.
fn build_registry(agent_bin: &str) -> Result<Registry> {
    let out = std::process::Command::new(agent_bin)
        .args(["--help-ai", "--json"])
        .output()
        .with_context(|| {
            format!(
                "running `{agent_bin} --help-ai --json` (is `agent` on PATH, or AGENT_BIN set?)"
            )
        })?;
    if !out.status.success() {
        bail!(
            "`{agent_bin} --help-ai --json` exited non-zero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let root: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing the agent catalog JSON")?;
    let specs = catalog::parse_catalog(&root)?;
    catalog::validate_read_classification(&specs)?;
    Ok(Registry::new(specs))
}

fn parse_listen(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix("--listen=") {
            return Some(v.to_string());
        }
        if a == "--listen" {
            return it.next().cloned();
        }
    }
    None
}

/// Parse `--allow-host <h>` (repeatable) / `--allow-host=<h>`, falling back to
/// `AGENT_MCP_ALLOWED_HOSTS` (comma-separated). Empty result = allow any Host
/// (the trusted-network default).
fn parse_allowed_hosts(args: &[String]) -> Vec<String> {
    let mut hosts = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(h) = a.strip_prefix("--allow-host=") {
            hosts.push(h.to_string());
        } else if a == "--allow-host" {
            if let Some(h) = it.next() {
                hosts.push(h.clone());
            }
        }
    }
    if hosts.is_empty() {
        if let Ok(env) = std::env::var("AGENT_MCP_ALLOWED_HOSTS") {
            hosts.extend(
                env.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }
    }
    hosts
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let allow_writes = args.iter().any(|a| a == "--allow-writes")
        || matches!(
            std::env::var("AGENT_MCP_ALLOW_WRITES").as_deref(),
            Ok("1") | Ok("true")
        );
    let listen = parse_listen(&args).or_else(|| std::env::var("AGENT_MCP_ADDR").ok());
    let allowed_hosts = parse_allowed_hosts(&args);

    let agent_bin = resolve_agent_bin();
    let reg = Arc::new(build_registry(&agent_bin)?);
    let (total, writes) = reg.counts();
    eprintln!(
        "agent-mcp: {total} tools ({writes} write-gated, {} read); writes {}; hosts {}",
        total - writes,
        if allow_writes { "ENABLED" } else { "disabled" },
        if allowed_hosts.is_empty() {
            "any (trusted network)".to_string()
        } else {
            allowed_hosts.join(",")
        }
    );
    let agent_bin = Arc::new(agent_bin);

    match listen {
        Some(addr) => serve_http(&addr, reg, allow_writes, agent_bin, allowed_hosts).await,
        None => {
            use rmcp::ServiceExt;
            eprintln!("agent-mcp: stdio");
            let server = AgentMcpServer {
                reg,
                allow_writes,
                agent_bin,
            };
            let running = server.serve(rmcp::transport::stdio()).await?;
            running.waiting().await?;
            Ok(())
        }
    }
}

async fn serve_http(
    addr: &str,
    reg: Arc<Registry>,
    allow_writes: bool,
    agent_bin: Arc<String>,
    allowed_hosts: Vec<String>,
) -> Result<()> {
    use axum::Router;
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, tower::StreamableHttpService,
        StreamableHttpServerConfig,
    };

    // Host allow-list (rmcp validates the inbound Host header). EMPTY = allow
    // any Host — the right default for a trusted-network bind, where remote
    // clients connect by the server's LAN IP/hostname. rmcp's own default is
    // localhost-only, which 403s every remote client even on a 0.0.0.0 bind;
    // that is the bug this fixes. Lock a public bind down with --allow-host /
    // AGENT_MCP_ALLOWED_HOSTS.
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts);

    let service: StreamableHttpService<AgentMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || {
                Ok(AgentMcpServer {
                    reg: reg.clone(),
                    allow_writes,
                    agent_bin: agent_bin.clone(),
                })
            },
            LocalSessionManager::default().into(),
            config,
        );

    let app = Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    eprintln!("agent-mcp: listening on http://{addr}/mcp");
    axum::serve(listener, app).await?;
    Ok(())
}
