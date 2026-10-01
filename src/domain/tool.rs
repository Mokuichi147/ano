//! Tool definitions and the per-call execution context.

use crate::domain::{environment::CheckConfig, plan::TASK_PLAN_NAME};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fmt, path::PathBuf, sync::Arc};

/// Reserved internal tool used to lazily discover registered tools.
pub const TOOL_SEARCH_NAME: &str = "tool_search";

/// Reserved runtime tool that hands a focused task to a sub-agent.
pub const DELEGATE_TASK_NAME: &str = "delegate_task";

/// Prefix reserved for aliases of directly connected MCP tools.
pub const DIRECT_MCP_PREFIX: &str = "mcp__";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default = "default_strict")]
    pub strict: bool,
    /// Ask the run's approval handler before every call, as for MCP tools
    /// that require approval. Not sent to the model.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub requires_approval: bool,
    /// Offer this tool in every request, without a `tool_search` first. For
    /// small tools that the instructions ask the model to use directly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub always_offered: bool,
    /// Each call acts on what its arguments name, such as a file to read or
    /// a command to run, so calling it for one thing after another is
    /// progress: only a round that repeats the calls of the round before
    /// exactly counts toward a loop. Not sent to the model.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub targeted: bool,
    /// When the tool can run, and how long a call may take. Not sent to the
    /// model.
    #[serde(skip)]
    pub runtime: ToolRuntime,
}

/// Whether a tool can run in a context.
pub type AvailabilityFn = dyn Fn(&ToolContext) -> bool + Send + Sync;
/// The seconds one call may take, from its arguments and context.
pub type DeadlineFn = dyn Fn(&Value, &ToolContext) -> Option<u64> + Send + Sync;

/// The rules of a tool that depend on the context or arguments of a call.
#[derive(Clone, Default)]
pub struct ToolRuntime {
    availability: Option<Arc<AvailabilityFn>>,
    deadline: Option<Arc<DeadlineFn>>,
}

impl fmt::Debug for ToolRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRuntime")
            .field("availability", &self.availability.is_some())
            .field("deadline", &self.deadline.is_some())
            .finish()
    }
}

fn default_strict() -> bool {
    true
}

impl ToolDefinition {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            strict: true,
            requires_approval: false,
            always_offered: false,
            targeted: false,
            runtime: ToolRuntime::default(),
        }
    }

    /// Require approval before each call of this tool.
    pub fn with_approval(mut self) -> Self {
        self.requires_approval = true;
        self
    }

    /// Offer this tool without a `tool_search` first.
    pub fn always_offered(mut self) -> Self {
        self.always_offered = true;
        self
    }

    /// Mark the tool as acting on what its arguments name; see `targeted`.
    pub fn targeted(mut self) -> Self {
        self.targeted = true;
        self
    }

    /// Run the tool only in contexts where `available` holds. A tool that
    /// cannot run is neither offered to the model nor sent for approval.
    pub fn available_when(
        mut self,
        available: impl Fn(&ToolContext) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.runtime.availability = Some(Arc::new(available));
        self
    }

    /// Let one call take the seconds `deadline` gives instead of the run's
    /// `tool_timeout_secs`, for a tool that enforces its own deadline and
    /// keeps partial output when it fires. Leave the tool a few seconds past
    /// its own deadline to report.
    pub fn with_deadline(
        mut self,
        deadline: impl Fn(&Value, &ToolContext) -> Option<u64> + Send + Sync + 'static,
    ) -> Self {
        self.runtime.deadline = Some(Arc::new(deadline));
        self
    }

    /// `arguments` without the top-level properties the schema does not
    /// declare, when it allows no others (a schema without `properties`
    /// declares none). Fields that the runtime adds to a call, such as the
    /// reviewed files of `git_commit_push`, then cannot come from the model.
    pub fn declared_arguments(&self, mut arguments: Value) -> Value {
        if self.parameters["additionalProperties"] == false {
            let declared = &self.parameters["properties"];
            if let Some(arguments) = arguments.as_object_mut() {
                arguments.retain(|name, _| declared.get(name).is_some());
            }
        }
        arguments
    }

    /// Whether the tool can run in `context` at all.
    pub fn can_run(&self, context: &ToolContext) -> bool {
        self.runtime
            .availability
            .as_ref()
            .is_none_or(|available| available(context))
    }

    /// The seconds a call with `arguments` may take, when the tool sets them.
    pub fn deadline_secs(&self, arguments: &Value, context: &ToolContext) -> Option<u64> {
        self.runtime
            .deadline
            .as_ref()
            .and_then(|deadline| deadline(arguments, context))
    }

    pub fn as_response_tool(&self) -> Value {
        serde_json::json!({
            "type": "function",
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
            "strict": self.strict,
        })
    }
}

/// Runtime information supplied to a tool invocation.
///
/// A webhook can select a named environment, and the selected environment is
/// represented here instead of trusting a path or permission sent by the
/// webhook caller.
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    pub user_id: String,
    pub environment: String,
    pub workspace: Option<PathBuf>,
    pub allow_writes: bool,
    /// Whether `workspace_exec` may run commands in the workspace.
    pub allow_exec: bool,
    /// Whether `web_fetch` may read public web pages.
    pub allow_web: bool,
    pub checks: BTreeMap<String, CheckConfig>,
}

/// Enforce the Responses API function name rules (`^[A-Za-z0-9_-]{1,64}$`).
/// `:` is rejected so local names never collide with `server:tool` policy
/// rules for MCP tools.
pub fn validate_tool_name(name: &str) -> Result<()> {
    if name == TASK_PLAN_NAME {
        bail!("tool name '{name}' is reserved for task planning");
    }
    if name == TOOL_SEARCH_NAME {
        bail!("tool name '{TOOL_SEARCH_NAME}' is reserved for lazy discovery");
    }
    if name == DELEGATE_TASK_NAME {
        bail!("tool name '{name}' is reserved for sub-agents");
    }
    if name.starts_with(DIRECT_MCP_PREFIX) {
        bail!("tool name prefix '{DIRECT_MCP_PREFIX}' is reserved for MCP tools");
    }
    if name.is_empty() {
        bail!("tool name cannot be empty");
    }
    if name.len() > 64 {
        bail!("tool name '{}' is longer than 64 characters", name);
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        bail!(
            "tool name '{}' may contain only ASCII letters, digits, '_' and '-'",
            name
        );
    }
    Ok(())
}
