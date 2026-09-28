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

/// Part of an assistant message, reported while a response is generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseDelta<'a> {
    /// More text of the current message (`output_text` or `refusal`).
    Text(&'a str),
    /// The current message is complete.
    MessageDone,
    /// The model is reasoning before it answers. Carries the reasoning text
    /// or summary when the endpoint streams it.
    Reasoning(&'a str),
}

/// Receives the deltas of a streamed response.
pub type DeltaSink<'a> = &'a (dyn Fn(ResponseDelta<'_>) + Send + Sync);

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

    /// `POST /responses`, reporting message text to `on_delta` while it is
    /// generated. Returns the same completed response as `create_response`.
    ///
    /// When this returns `Ok`, every message in the response has been passed
    /// to `on_delta`, whether or not the endpoint streamed it. The default
    /// waits for the whole response and then reports its messages at once.
    async fn create_response_streaming(
        &self,
        payload: &Value,
        on_delta: DeltaSink<'_>,
    ) -> Result<Value> {
        let response = self.create_response(payload).await?;
        replay_deltas(&response, on_delta);
        Ok(response)
    }

    /// Whether the endpoint implements `/responses/compact`. Other endpoints
    /// compact the history by asking the model for a summary instead.
    fn supports_remote_compaction(&self) -> bool {
        false
    }

    /// サーバーに応答を保存しない接続先では、毎回履歴全体を送信する。
    fn requires_full_history(&self) -> bool {
        false
    }

    /// The models the endpoint offers (`GET /models`), sorted by name, or
    /// `None` when it does not list them (such as a ChatGPT subscription).
    async fn list_models(&self) -> Result<Option<Vec<String>>> {
        Ok(None)
    }
}

/// Report the messages of a completed response to `on_delta`, one delta per
/// message, for an endpoint that did not stream them.
pub fn replay_deltas(response: &Value, on_delta: DeltaSink<'_>) {
    let messages = response["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "message")
        .map(|item| {
            item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str().or_else(|| part["refusal"].as_str()))
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let messages = if messages.is_empty() {
        // Some compatible endpoints return only the aggregate text.
        response["output_text"]
            .as_str()
            .map(|text| vec![text.to_string()])
            .unwrap_or_default()
    } else {
        messages
    };
    for text in messages.iter().filter(|text| !text.trim().is_empty()) {
        on_delta(ResponseDelta::Text(text));
        on_delta(ResponseDelta::MessageDone);
    }
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

    async fn create_response_streaming(
        &self,
        payload: &Value,
        on_delta: DeltaSink<'_>,
    ) -> Result<Value> {
        (**self).create_response_streaming(payload, on_delta).await
    }

    fn supports_remote_compaction(&self) -> bool {
        (**self).supports_remote_compaction()
    }

    fn requires_full_history(&self) -> bool {
        (**self).requires_full_history()
    }

    async fn list_models(&self) -> Result<Option<Vec<String>>> {
        (**self).list_models().await
    }
}

/// Durable storage of one conversation. Every method that changes the
/// conversation must persist it before returning, so a crash never loses
/// a recorded tool call or result.
pub trait ConversationStore: Send + Sync {
    fn data(&self) -> &SessionData;
    /// false のストアは原文の記録だけに使い、次のリクエストはその履歴から組み立てない。
    fn replays_history(&self) -> bool {
        true
    }
    fn set_history_parent(&mut self, _conversation: Option<&str>, _call_id: Option<&str>) {}
    fn record_control_input(&mut self, _text: &str) -> Result<()> {
        Ok(())
    }
    fn begin_turn(&mut self, input: &Value) -> Result<()>;
    /// 原文と内部指示を分けて受け取る。既存ストアはモデル用の入力だけを保持する。
    fn begin_turn_with_source(
        &mut self,
        input: &Value,
        _original: &Value,
        _runtime: &Value,
        _origin: &str,
    ) -> Result<()> {
        self.begin_turn(input)
    }
    /// Must be durable before any call in `output` is executed.
    fn record_response(&mut self, id: &str, output: &[Value]) -> Result<()>;
    fn checkpoint_tool_result(&mut self, result: &Value, plan: &TaskPlan) -> Result<()>;
    /// モデル用に短縮する前のツール結果を原文履歴へ渡す。
    fn checkpoint_tool_result_with_raw(
        &mut self,
        result: &Value,
        _raw: &Value,
        plan: &TaskPlan,
    ) -> Result<()> {
        self.checkpoint_tool_result(result, plan)
    }
    /// Replace the task plan, e.g. when the user sets or clears a goal.
    fn replace_plan(&mut self, plan: &TaskPlan) -> Result<()>;
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
    /// 以後の応答を別のモデル・接続先から受け取る。
    /// 詳細は [`crate::domain::session::SessionData::switch_model`]。
    fn switch_model(
        &mut self,
        _choice: &crate::domain::session::ModelChoice,
        _endpoint: &str,
    ) -> Result<()> {
        anyhow::bail!("this conversation store cannot switch models")
    }
}

/// 実行用の会話ストアに、圧縮されない原文の記録と非同期の同期を追加する。
#[async_trait]
pub trait HistoryBackend: Send + Sync {
    /// セッションなしの実行を記録するための、`replays_history` が false のストア。
    fn transcript_store(
        &self,
        binding: crate::domain::session::SessionBinding,
    ) -> Box<dyn ConversationStore>;
    fn wrap<'a>(
        &self,
        store: &'a mut dyn ConversationStore,
    ) -> Result<Box<dyn ConversationStore + 'a>>;
    async fn sync(&self, user_id: &str) -> Result<Value>;
}

/// What an approval request is for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ApprovalSource {
    /// A tool on an MCP server (`server_label` names the server).
    #[default]
    Mcp,
    /// A local tool registered with `ToolDefinition::with_approval`, such as
    /// `workspace_exec`. `server_label` is empty.
    LocalTool,
}

/// A request to approve one tool call. Despite the name, it also covers local
/// tools that require approval; see `source`.
#[derive(Debug, Clone, Default)]
pub struct McpApprovalRequest {
    pub approval_request_id: String,
    pub source: ApprovalSource,
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

impl McpApprovalRequest {
    /// The tool as shown to the user: `server:tool` for MCP, the name for
    /// local tools.
    pub fn target(&self) -> String {
        match self.source {
            ApprovalSource::Mcp => format!("{}:{}", self.server_label, self.tool_name),
            ApprovalSource::LocalTool => self.tool_name.clone(),
        }
    }

    /// A heading for approval prompts.
    pub fn heading(&self) -> &'static str {
        match self.source {
            ApprovalSource::Mcp => "MCP approval requested",
            ApprovalSource::LocalTool => "Tool approval requested",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDecision {
    pub approved: bool,
    /// Explanation shown to the user and, for a denial, to the model.
    pub reason: Option<String>,
}

/// Decides whether an MCP tool call, or a local tool that requires approval,
/// may run.
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
