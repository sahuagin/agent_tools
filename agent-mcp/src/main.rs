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

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let allow_writes = args.iter().any(|a| a == "--allow-writes")
        || matches!(
            std::env::var("AGENT_MCP_ALLOW_WRITES").as_deref(),
            Ok("1") | Ok("true")
        );
    let listen = parse_listen(&args).or_else(|| std::env::var("AGENT_MCP_ADDR").ok());

    let agent_bin = resolve_agent_bin();
    let reg = Arc::new(build_registry(&agent_bin)?);
    let (total, writes) = reg.counts();
    eprintln!(
        "agent-mcp: {total} tools ({writes} write-gated, {} read); writes {}",
        total - writes,
        if allow_writes { "ENABLED" } else { "disabled" }
    );
    let agent_bin = Arc::new(agent_bin);

    match listen {
        Some(addr) => serve_http(&addr, reg, allow_writes, agent_bin).await,
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
) -> Result<()> {
    use axum::Router;
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, tower::StreamableHttpService,
        StreamableHttpServerConfig,
    };

    // allowed_hosts must include whatever Host header clients send. Unlike a
    // localhost-only service, agent-mcp is meant to be reachable on its bind
    // address — so allow-list it (and its host part), not just loopback.
    let host = addr.rsplit_once(':').map(|(h, _)| h.to_string());
    let mut config = StreamableHttpServerConfig::default();
    let mut allowed = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
        addr.to_string(),
    ];
    if let Some(h) = host {
        allowed.push(h);
    }
    config.allowed_hosts = allowed;

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
