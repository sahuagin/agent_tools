//! The dynamic rmcp `ServerHandler`: advertises the introspected tools and
//! dispatches calls to the `agent` subprocess.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::*;
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::{Map, Value};

use crate::catalog::ToolSpec;
use crate::{invoke, schema};

/// Holds the tool specs and an index by name.
pub struct Registry {
    specs: Vec<ToolSpec>,
    by_name: HashMap<String, usize>,
}

impl Registry {
    pub fn new(specs: Vec<ToolSpec>) -> Self {
        let by_name = specs
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.clone(), i))
            .collect();
        Self { specs, by_name }
    }

    /// rmcp `Tool` list, write tools omitted unless `allow_writes`.
    pub fn tools(&self, allow_writes: bool) -> Vec<Tool> {
        self.specs
            .iter()
            .filter(|s| allow_writes || !s.is_write)
            .map(|s| {
                let schema = match schema::input_schema(s) {
                    Value::Object(m) => Arc::new(m),
                    _ => Arc::new(Map::new()),
                };
                Tool::new(s.name.clone(), s.description.clone(), schema)
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.by_name.get(name).map(|i| &self.specs[*i])
    }

    /// (total, write-gated) counts, for the startup banner.
    pub fn counts(&self) -> (usize, usize) {
        let total = self.specs.len();
        let writes = self.specs.iter().filter(|s| s.is_write).count();
        (total, writes)
    }
}

#[derive(Clone)]
pub struct AgentMcpServer {
    pub reg: Arc<Registry>,
    pub allow_writes: bool,
    pub agent_bin: Arc<String>,
}

impl ServerHandler for AgentMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agent-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Exposes the `agent` CLI (memory / tasks / metrics) as MCP tools, \
                 introspected from `agent --help-ai --json`. Mutating tools are \
                 hidden and refused unless the server was started with --allow-writes.",
            )
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        let tools = self.reg.tools(self.allow_writes);
        async move {
            Ok(ListToolsResult {
                tools,
                ..Default::default()
            })
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        let reg = self.reg.clone();
        let allow_writes = self.allow_writes;
        let agent_bin = self.agent_bin.clone();
        async move {
            let Some(spec) = reg.get(&request.name) else {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "unknown tool: {}",
                    request.name
                ))]));
            };

            // Write-gate enforced HERE (rmcp does no list-membership check on
            // call_tool), not only via the list_tools filter.
            if spec.is_write && !allow_writes {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "tool '{}' mutates state; start agent-mcp with --allow-writes to enable it",
                    spec.name
                ))]));
            }

            let args = request.arguments.unwrap_or_default();
            let argv = match invoke::build_argv(spec, &args) {
                Ok(a) => a,
                Err(e) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "bad arguments: {e}"
                    ))]))
                }
            };

            let bin = (*agent_bin).clone();
            let res = tokio::task::spawn_blocking(move || invoke::run_agent(&bin, &argv))
                .await
                .map_err(|e| McpError::internal_error(format!("task join: {e}"), None))?;

            match res {
                Ok((true, stdout, _stderr)) => {
                    Ok(CallToolResult::success(vec![Content::text(stdout)]))
                }
                Ok((false, stdout, stderr)) => Ok(CallToolResult::error(vec![Content::text(
                    format!("agent exited non-zero.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"),
                )])),
                Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                    "failed to run agent: {e}"
                ))])),
            }
        }
    }
}
