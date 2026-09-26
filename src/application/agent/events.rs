use crate::domain::{
    compaction::CompactionRecord,
    plan::TaskPlan,
    usage::{ApiOperation, StopReason, UsageSummary},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    ContextCompacted {
        round: usize,
        record: CompactionRecord,
    },
    UsageUpdated {
        round: usize,
        operation: ApiOperation,
        usage: UsageSummary,
    },
    ExecutionStopped {
        reason: StopReason,
        usage: UsageSummary,
    },
    PlanUpdated {
        round: usize,
        plan: TaskPlan,
    },
    AssistantProgress {
        round: usize,
        text: String,
    },
    /// Summary of the model's reasoning, when `reasoning_summary` is set.
    ReasoningSummary {
        round: usize,
        text: String,
    },
    LocalToolCall {
        round: usize,
        name: String,
        arguments: Value,
    },
    LocalToolResult {
        round: usize,
        name: String,
        output: Value,
    },
    LocalToolBlocked {
        round: usize,
        name: String,
    },
    McpToolCall {
        round: usize,
        server_label: String,
        tool_name: String,
        arguments: Value,
    },
    McpToolResult {
        round: usize,
        server_label: String,
        tool_name: String,
        output: Value,
    },
    McpToolBlocked {
        round: usize,
        server_label: String,
        tool_name: String,
    },
    McpApproval {
        round: usize,
        server_label: String,
        tool_name: String,
        approved: bool,
        /// Present when the decision was explained, e.g. by automatic review.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    ToolSearch {
        round: usize,
        query: String,
        results: Vec<Value>,
    },
}

/// Receives each event as soon as it happens, so progress is visible during a
/// long run and is not lost when the run fails.
pub type EventListener = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

/// Collects events from concurrently running tool calls. The listener is
/// called under the same lock, so it sees events one at a time and in the
/// same order as `AgentResult::events`.
pub(super) struct EventLog<'a> {
    events: Mutex<Vec<AgentEvent>>,
    listener: Option<&'a EventListener>,
}

impl<'a> EventLog<'a> {
    pub(super) fn new(listener: Option<&'a EventListener>) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            listener,
        }
    }

    pub(super) fn push(&self, event: AgentEvent) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(listener) = self.listener {
            listener(&event);
        }
        events.push(event);
    }

    pub(super) fn into_events(self) -> Vec<AgentEvent> {
        self.events
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
