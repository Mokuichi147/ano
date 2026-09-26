//! Tool definitions and the per-call execution context.

use crate::domain::{environment::CheckConfig, plan::TASK_PLAN_NAME};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};

/// Reserved internal tool used to lazily discover registered tools.
pub const TOOL_SEARCH_NAME: &str = "tool_search";

/// Prefix reserved for aliases of directly connected MCP tools.
pub const DIRECT_MCP_PREFIX: &str = "mcp__";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default = "default_strict")]
    pub strict: bool,
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
        }
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
