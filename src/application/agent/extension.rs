//! Extension points through which a harness adds its own runtime tools and
//! rules to the run loop, which does not know them.

use super::{dispatch::RoundScope, Agent, AgentResult};
use crate::{
    application::registry::ToolRegistry,
    domain::tool::{ToolContext, ToolDefinition},
};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::{future::Future, pin::Pin};

/// Runtime tools and tool-call rules a harness adds to an agent.
///
/// Its tools are handled by the extension rather than the tool registry, so
/// they can use the agent itself, for example to start a sub-agent. They are
/// offered without a `tool_search` and take precedence over registered tools
/// of the same name.
#[async_trait]
pub trait AgentExtension: Send + Sync {
    /// Every runtime tool the extension handles.
    fn tools(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    /// Whether its tool `name` is offered to `run`. A call of a tool that is
    /// not offered is refused without reaching `call_tool`.
    fn offers(&self, _name: &str, _run: &RunInfo<'_>) -> bool {
        true
    }

    /// Run a call of one of its tools. Failures go back to the model as the
    /// output, like those of local tools.
    async fn call_tool(&self, name: &str, arguments: &Value, call: ExtensionCall<'_>) -> Value;

    /// Look at a call of a registered local tool before it is approved and
    /// run. `Some(output)` refuses the call with that output.
    async fn check_call(
        &self,
        _name: &str,
        _arguments: &Value,
        _run: &RunInfo<'_>,
    ) -> Option<Value> {
        None
    }

    /// The arguments an approved call of a registered local tool runs with.
    fn prepare_call(&self, _name: &str, arguments: Value, _run: &RunInfo<'_>) -> Value {
        arguments
    }
}

/// The run a tool call belongs to.
#[derive(Clone, Copy)]
pub struct RunInfo<'a> {
    /// 0 for the caller's run, 1 for a sub-agent.
    pub depth: usize,
    pub context: &'a ToolContext,
    /// The agent's local tools, which an extension may call directly.
    pub registry: &'a ToolRegistry,
}

/// A call of an extension's tool, with what it may use of the agent.
#[derive(Clone, Copy)]
pub struct ExtensionCall<'a> {
    pub(super) agent: &'a Agent,
    pub(super) scope: RoundScope<'a>,
}

impl<'a> ExtensionCall<'a> {
    pub fn run(&self) -> RunInfo<'a> {
        RunInfo {
            depth: self.scope.depth,
            context: self.scope.tool_context,
            registry: &self.agent.registry,
        }
    }

    /// Run a sub-agent in a fresh conversation and return its result. Its
    /// usage counts toward the calling run, and a sub-agent cannot start
    /// another one.
    pub fn run_subagent(
        &self,
        spec: SubagentSpec,
    ) -> Pin<Box<dyn Future<Output = Result<AgentResult>> + Send + 'a>> {
        let (agent, scope) = (self.agent, self.scope);
        Box::pin(async move { agent.run_subagent(spec, scope).await })
    }
}

/// A sub-agent to start: its task and how it runs.
#[derive(Debug, Clone)]
pub struct SubagentSpec {
    /// The user message of its conversation.
    pub task: String,
    pub context: ToolContext,
    /// Appended to the agent's instructions.
    pub instructions: String,
    /// The role in `SubagentModels` whose model it runs on; without a model
    /// for the role, the agent's own.
    pub role: String,
    /// Deny every call that needs approval, with this reason.
    pub deny_approvals: Option<String>,
}
