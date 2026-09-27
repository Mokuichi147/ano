//! Conversation state and its transitions. Tool calls are journaled before
//! execution; interrupted calls are never replayed automatically.
//!
//! Persistence (locking, atomic saves, archives) belongs to the store that
//! holds this data. Every transition here only changes memory.

use crate::domain::{
    compaction::CompactionRecord, plan::TaskPlan, tool::ToolContext, usage::UsageSummary,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

pub const SESSION_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionBinding {
    pub user_id: String,
    pub environment: String,
    pub workspace: Option<PathBuf>,
    pub endpoint: String,
}

impl SessionBinding {
    pub fn new(context: &ToolContext, endpoint: &str) -> Result<Self> {
        Ok(Self {
            user_id: context.user_id.clone(),
            environment: context.environment.clone(),
            workspace: context
                .workspace
                .as_ref()
                .map(std::fs::canonicalize)
                .transpose()?,
            endpoint: endpoint.trim_end_matches('/').to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Ready,
    Running,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionData {
    pub version: u32,
    /// 圧縮やプロセス再起動後も原文履歴と対応する会話 ID。
    #[serde(default = "new_conversation_id")]
    pub conversation_id: String,
    pub binding: SessionBinding,
    pub status: SessionStatus,
    pub completed_turns: u64,
    pub updated_at_unix: u64,
    pub last_response_id: Option<String>,
    pub last_error: Option<String>,
    pub history: Vec<Value>,
    #[serde(default)]
    pub plan: TaskPlan,
    #[serde(default)]
    pub usage: UsageSummary,
    #[serde(default)]
    pub compactions: Vec<CompactionRecord>,
    pending_calls: Vec<Value>,
}

impl SessionData {
    pub fn new(binding: SessionBinding, now_unix: u64) -> Self {
        Self {
            version: SESSION_VERSION,
            conversation_id: new_conversation_id(),
            binding,
            status: SessionStatus::Ready,
            completed_turns: 0,
            updated_at_unix: now_unix,
            last_response_id: None,
            last_error: None,
            history: Vec::new(),
            plan: TaskPlan::default(),
            usage: UsageSummary::default(),
            compactions: Vec::new(),
            pending_calls: Vec::new(),
        }
    }

    /// Calls from the last response whose results are not recorded yet.
    pub fn pending_calls(&self) -> &[Value] {
        &self.pending_calls
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != SESSION_VERSION {
            bail!("unsupported session version {}", self.version);
        }
        self.plan.validate().context("invalid session plan")
    }

    pub fn begin_turn(&mut self, input: &Value) -> Result<()> {
        if self.status == SessionStatus::Running {
            bail!("session already has an active turn");
        }
        self.history.extend(
            input
                .as_array()
                .context("session input must be an array")?
                .iter()
                .cloned(),
        );
        self.status = SessionStatus::Running;
        self.last_error = None;
        Ok(())
    }

    /// The store must persist this before executing any of the returned calls.
    pub fn record_response(&mut self, id: &str, output: &[Value]) {
        self.history.extend_from_slice(output);
        self.last_response_id = Some(id.to_string());
        self.pending_calls = output
            .iter()
            .filter(|item| {
                matches!(
                    item["type"].as_str(),
                    Some("function_call" | "mcp_approval_request")
                )
            })
            .cloned()
            .collect();
    }

    pub fn record_tool_results(&mut self, results: &[Value]) {
        for result in results {
            self.pending_calls
                .retain(|call| !matches_result(call, result));
        }
        self.history.extend_from_slice(results);
    }

    pub fn checkpoint_tool_result(&mut self, result: &Value, plan: &TaskPlan) {
        self.plan = plan.clone();
        self.record_tool_results(std::slice::from_ref(result));
    }

    pub fn replace_plan(&mut self, plan: &TaskPlan) {
        self.plan = plan.clone();
    }

    pub fn record_runtime_input(&mut self, input: &Value) -> Result<()> {
        self.history.extend(
            input
                .as_array()
                .context("runtime input must be an array")?
                .iter()
                .cloned(),
        );
        Ok(())
    }

    pub fn complete(&mut self) {
        self.status = SessionStatus::Ready;
        self.completed_turns += 1;
    }

    pub fn record_usage(&mut self, delta: &UsageSummary) {
        self.usage.add(delta);
    }

    pub fn ensure_compactable(&self) -> Result<()> {
        if !self.pending_calls.is_empty() {
            bail!("cannot compact while tool results are pending");
        }
        Ok(())
    }

    pub fn apply_compaction(
        &mut self,
        history: Vec<Value>,
        record: CompactionRecord,
    ) -> Result<()> {
        self.ensure_compactable()?;
        self.history = history;
        self.compactions.push(record);
        Ok(())
    }

    /// Close pending calls without running them, e.g. at a token budget.
    pub fn skip_pending(&mut self, message: &str) {
        let results = self.pending_calls.iter().map(|call| {
            if call["type"] == "function_call" {
                json!({"type":"function_call_output","call_id":call.get("call_id").or_else(|| call.get("id")),
                    "output":json!({"error":"execution_limit","message":message}).to_string()})
            } else {
                json!({"type":"mcp_approval_response","approval_request_id":call.get("approval_request_id").or_else(|| call.get("id")),"approve":false})
            }
        }).collect::<Vec<_>>();
        self.record_tool_results(&results);
    }

    /// Mark the turn failed. Pending calls get an "outcome unknown" result so
    /// the model never assumes they ran, and they are never replayed.
    pub fn fail(&mut self, error: &str) -> Result<()> {
        for call in self.pending_calls.drain(..) {
            if call["type"] == "function_call" {
                let call_id = call
                    .get("call_id")
                    .or_else(|| call.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                self.history.push(json!({"type":"function_call_output", "call_id":call_id,
                    "output":serde_json::to_string(&json!({"error":"execution_interrupted", "message":"Outcome unknown. This call was not replayed. Inspect current state before repeating any side effects."}))?}));
            } else {
                let id = call
                    .get("approval_request_id")
                    .or_else(|| call.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                self.history.push(json!({"type":"mcp_approval_response", "approval_request_id":id, "approve":false}));
            }
        }
        self.last_error = Some(error.to_string());
        self.history.push(json!({"role":"user", "content":[{"type":"input_text", "text":format!("Agent execution stopped: {error}")}]}));
        self.status = SessionStatus::Failed;
        Ok(())
    }
}

fn new_conversation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn matches_result(call: &Value, result: &Value) -> bool {
    let (kind, key) = match call["type"].as_str() {
        Some("function_call") => ("function_call_output", "call_id"),
        Some("mcp_approval_request") => ("mcp_approval_response", "approval_request_id"),
        _ => return false,
    };
    result["type"] == kind
        && call
            .get(key)
            .or_else(|| call.get("id"))
            .is_some_and(|id| !id.is_null() && Some(id) == result.get(key))
}
