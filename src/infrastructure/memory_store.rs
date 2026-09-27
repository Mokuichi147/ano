//! In-memory conversation store for multi-turn use within one process.

use crate::{
    application::ports::ConversationStore,
    domain::{
        compaction::CompactionRecord,
        plan::TaskPlan,
        session::{SessionBinding, SessionData},
        usage::UsageSummary,
    },
};
use anyhow::Result;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// Keeps a conversation for the lifetime of the process, e.g. an interactive
/// chat without `--session`. Nothing is written to disk.
pub struct MemoryConversation {
    data: SessionData,
}

impl MemoryConversation {
    pub fn new(binding: SessionBinding) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            data: SessionData::new(binding, now),
        }
    }
}

impl ConversationStore for MemoryConversation {
    fn data(&self) -> &SessionData {
        &self.data
    }

    fn begin_turn(&mut self, input: &Value) -> Result<()> {
        self.data.begin_turn(input)
    }

    fn record_response(&mut self, id: &str, output: &[Value]) -> Result<()> {
        self.data.record_response(id, output);
        Ok(())
    }

    fn checkpoint_tool_result(&mut self, result: &Value, plan: &TaskPlan) -> Result<()> {
        self.data.checkpoint_tool_result(result, plan);
        Ok(())
    }

    fn replace_plan(&mut self, plan: &TaskPlan) -> Result<()> {
        self.data.replace_plan(plan);
        Ok(())
    }

    fn record_runtime_input(&mut self, input: &Value) -> Result<()> {
        self.data.record_runtime_input(input)
    }

    fn record_usage(&mut self, delta: &UsageSummary) -> Result<()> {
        self.data.record_usage(delta);
        Ok(())
    }

    fn replace_history(
        &mut self,
        history: Vec<Value>,
        record: CompactionRecord,
    ) -> Result<CompactionRecord> {
        self.data.apply_compaction(history, record.clone())?;
        Ok(record)
    }

    fn skip_pending(&mut self, message: &str) -> Result<()> {
        self.data.skip_pending(message);
        Ok(())
    }

    fn complete(&mut self) -> Result<()> {
        self.data.complete();
        Ok(())
    }

    fn fail(&mut self, error: &str) -> Result<()> {
        self.data.fail(error)
    }
}
