//! Tool definitions and the per-call execution context.

use crate::domain::{environment::CheckConfig, plan::TASK_PLAN_NAME};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};

/// Reserved internal tool used to lazily discover registered tools.
pub const TOOL_SEARCH_NAME: &str = "tool_search";

/// Reserved runtime tool that hands a focused task to a sub-agent.
pub const DELEGATE_TASK_NAME: &str = "delegate_task";

/// Built-in tool that runs shell commands when `ToolContext::allow_exec` is set.
pub const WORKSPACE_EXEC_NAME: &str = "workspace_exec";

/// Built-in tool that reads a workspace file by offset or by lines.
pub const WORKSPACE_READ_NAME: &str = "workspace_read";

/// Built-in tools that change one workspace file.
pub const WORKSPACE_EDIT_NAME: &str = "workspace_edit";
pub const WORKSPACE_WRITE_NAME: &str = "workspace_write";
/// Built-in tool that reads web pages when `ToolContext::allow_web` is set.
pub const WEB_FETCH_NAME: &str = "web_fetch";
/// Built-in tool that commits workspace files and pushes them; it changes
/// the checkout, so it needs `ToolContext::allow_writes`.
pub const GIT_COMMIT_PUSH_NAME: &str = "git_commit_push";
/// Built-in tool that shows the uncommitted changes of the workspace, with a
/// content hash per file that reviews are recorded against.
pub const GIT_DIFF_NAME: &str = "git_diff";
/// Reserved runtime tool that has a read-only sub-agent in a fresh
/// conversation review the uncommitted changes. `git_commit_push` only
/// commits files in the state a review last saw.
pub const REVIEW_CHANGES_NAME: &str = "review_changes";

/// Default and maximum `timeout_secs` of one `workspace_exec` command.
pub const EXEC_DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const EXEC_MAX_TIMEOUT_SECS: u64 = 1800;

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

impl ToolContext {
    /// Whether a registered tool can run in this context at all. Tools that
    /// cannot are neither offered to the model nor sent for approval.
    pub fn can_run(&self, tool_name: &str) -> bool {
        match tool_name {
            WORKSPACE_EXEC_NAME => self.allow_exec,
            WEB_FETCH_NAME => self.allow_web,
            GIT_COMMIT_PUSH_NAME => self.allow_writes,
            _ => true,
        }
    }
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
    if name == DELEGATE_TASK_NAME || name == REVIEW_CHANGES_NAME {
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
