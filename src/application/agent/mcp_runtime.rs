use crate::{
    application::ports::{DirectMcpServer, DirectMcpTool},
    domain::policy::UserPolicy,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::sync::Arc;

/// The MCP connections one run may use, filtered by that run's policy.
#[derive(Default)]
pub(crate) struct McpRuntime {
    servers: Vec<Arc<dyn DirectMcpServer>>,
    policy: UserPolicy,
}

impl McpRuntime {
    pub fn new(servers: Vec<Arc<dyn DirectMcpServer>>, policy: UserPolicy) -> Self {
        Self { servers, policy }
    }

    /// Tools on the borrowed servers that the run's policy allows.
    pub fn tools(&self) -> impl Iterator<Item = (&dyn DirectMcpServer, &DirectMcpTool)> {
        self.servers.iter().flat_map(move |server| {
            server
                .tools()
                .iter()
                .filter(move |tool| server.config().is_tool_allowed(&self.policy, &tool.name))
                .map(move |tool| (server.as_ref(), tool))
        })
    }

    pub fn find_tool(&self, function_name: &str) -> Option<(&dyn DirectMcpServer, &DirectMcpTool)> {
        self.tools()
            .find(|(_, tool)| tool.function_name == function_name)
    }

    pub async fn call_tool(&self, function_name: &str, arguments: &Value) -> Result<Value> {
        let (server, tool) = self
            .find_tool(function_name)
            .with_context(|| format!("unknown direct MCP function '{function_name}'"))?;
        let label = &server.config().label;
        if !server.is_healthy() {
            anyhow::bail!(
                "connection to MCP server '{label}' was lost; it will be reconnected for the next task"
            );
        }
        let arguments = arguments.as_object().cloned().with_context(|| {
            format!(
                "arguments for MCP tool '{}' must be a JSON object",
                tool.name
            )
        })?;
        server
            .call_tool(&tool.name, arguments)
            .await
            .with_context(|| format!("MCP tool '{}' on server '{label}' failed", tool.name))
    }
}
