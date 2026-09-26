//! Ports: the interfaces the agent needs from the outside world.
//!
//! The application layer depends only on these traits. Adapters in
//! `infrastructure` (HTTP client, MCP connections, session files) and
//! `interface` (terminal prompts) implement them.

use crate::domain::{
    compaction::CompactionRecord, mcp::McpServerConfig, plan::TaskPlan, policy::UserPolicy,
    session::SessionData, usage::UsageSummary,
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::sync::Arc;

/// A Responses API compatible endpoint. Payloads and responses use the
/// Responses API JSON format, which is also the conversation history format.
#[async_trait]
pub trait ResponsesApi: Send + Sync {
    /// Identifies the endpoint a saved session is bound to.
    fn base_url(&self) -> &str;
    /// `POST /responses`
    async fn create_response(&self, payload: &Value) -> Result<Value>;
    /// `POST /responses/compact`
    async fn compact_response(&self, payload: &Value) -> Result<Value>;
}

#[async_trait]
impl<T: ResponsesApi + ?Sized> ResponsesApi for Arc<T> {
    fn base_url(&self) -> &str {
        (**self).base_url()
    }

    async fn create_response(&self, payload: &Value) -> Result<Value> {
        (**self).create_response(payload).await
    }

    async fn compact_response(&self, payload: &Value) -> Result<Value> {
        (**self).compact_response(payload).await
    }
}

/// Durable storage of one conversation. Every method that changes the
/// conversation must persist it before returning, so a crash never loses
/// a recorded tool call or result.
pub trait ConversationStore: Send + Sync {
    fn data(&self) -> &SessionData;
    fn begin_turn(&mut self, input: &Value) -> Result<()>;
    /// Must be durable before any call in `output` is executed.
    fn record_response(&mut self, id: &str, output: &[Value]) -> Result<()>;
    fn checkpoint_tool_result(&mut self, result: &Value, plan: &TaskPlan) -> Result<()>;
    fn record_runtime_input(&mut self, input: &Value) -> Result<()>;
    fn record_usage(&mut self, delta: &UsageSummary) -> Result<()>;
    /// Replace the history with a compacted window. Returns the record as
    /// stored, e.g. with the name of an archive of the previous history.
    fn replace_history(
        &mut self,
        history: Vec<Value>,
        record: CompactionRecord,
    ) -> Result<CompactionRecord>;
    fn skip_pending(&mut self, message: &str) -> Result<()>;
    fn complete(&mut self) -> Result<()>;
    fn fail(&mut self, error: &str) -> Result<()>;
}

#[derive(Debug, Clone, Default)]
pub struct McpApprovalRequest {
    pub approval_request_id: String,
    pub server_label: String,
    pub tool_name: String,
    pub arguments: Value,
    /// The tool's description, when known, to judge what the call does.
    pub tool_description: Option<String>,
    /// Text the user gave for the current run, to judge whether the call is
    /// within the request.
    pub user_request: String,
    /// Why an automatic review passed this request on to the user.
    pub review: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDecision {
    pub approved: bool,
    /// Explanation shown to the user and, for a denial, to the model.
    pub reason: Option<String>,
}

/// Decides whether an MCP tool call may run.
#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn approve(&self, request: McpApprovalRequest) -> Result<bool>;

    /// Decide with an optional reason. Override this to explain decisions;
    /// the default wraps `approve`.
    async fn decide(&self, request: McpApprovalRequest) -> Result<ApprovalDecision> {
        Ok(ApprovalDecision {
            approved: self.approve(request).await?,
            reason: None,
        })
    }
}

/// A tool offered by a directly connected MCP server.
#[derive(Debug, Clone)]
pub struct DirectMcpTool {
    /// Alias exposed to the model as a function name.
    pub function_name: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// One live connection to a directly connected (`stdio` / `streamable_http`)
/// MCP server.
#[async_trait]
pub trait DirectMcpServer: Send + Sync {
    fn config(&self) -> &McpServerConfig;
    /// Tools permitted by the server config. User policy is applied per run.
    fn tools(&self) -> &[DirectMcpTool];
    fn is_healthy(&self) -> bool;
    async fn call_tool(&self, tool_name: &str, arguments: Map<String, Value>) -> Result<Value>;
}

/// Provides MCP connections to runs.
#[async_trait]
pub trait McpGateway: Send + Sync {
    /// Every configured server, including Responses-managed ones.
    fn configs(&self) -> &[McpServerConfig];
    /// Connections to the direct servers a run with `policy` may use. Servers
    /// disabled for the policy must not be connected at all.
    async fn connect(&self, policy: &UserPolicy) -> Result<Vec<Arc<dyn DirectMcpServer>>>;
    /// Close connections and stop server processes.
    async fn shutdown(&self);
}
