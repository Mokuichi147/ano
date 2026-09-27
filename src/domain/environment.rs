//! Named execution environments and their trusted validation checks.

use crate::domain::approval::ApprovalMode;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

/// An environment profile used by webhook jobs and CLI runs.
///
/// Callers select the profile by name; they cannot submit an arbitrary path or
/// tool list in the webhook payload.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnvironmentConfig {
    pub workspace: Option<PathBuf>,
    pub model: Option<String>,
    pub instructions: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub disabled_tools: Vec<String>,
    pub allow_writes: bool,
    /// Let `workspace_exec` run arbitrary commands in the workspace. Every
    /// command still goes through the approval mode.
    pub allow_exec: bool,
    /// Let `web_fetch` read public web pages. Every fetch still goes through
    /// the approval mode, since a URL can carry data out.
    pub allow_web: bool,
    /// Shorthand for `approval_mode = "allow"`. Kept for existing configs.
    pub auto_approve_mcp: bool,
    /// How MCP approval requests are answered in this environment. Defaults
    /// to `allow` with `auto_approve_mcp`, otherwise `deny`, because webhook
    /// jobs have nobody to ask.
    pub approval_mode: Option<ApprovalMode>,
    /// Explicitly trusted validation programs. Arguments are fixed by config.
    pub checks: BTreeMap<String, CheckConfig>,
}

impl EnvironmentConfig {
    /// The approval mode after applying the `auto_approve_mcp` shorthand.
    pub fn effective_approval_mode(&self) -> ApprovalMode {
        match (self.approval_mode, self.auto_approve_mcp) {
            (Some(mode), _) => mode,
            (None, true) => ApprovalMode::Allow,
            (None, false) => ApprovalMode::Deny,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.auto_approve_mcp && self.approval_mode.is_some() {
            bail!("set either approval_mode or auto_approve_mcp, not both");
        }
        if self
            .model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty())
        {
            bail!("model must not be empty");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckConfig {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_check_timeout")]
    pub timeout_secs: u64,
}

fn default_check_timeout() -> u64 {
    90
}

impl CheckConfig {
    pub fn validate(&self) -> Result<()> {
        if self.program.trim().is_empty() {
            bail!("check.program must not be empty");
        }
        if self.timeout_secs == 0 || self.timeout_secs > 3600 {
            bail!("check.timeout_secs must be between 1 and 3600");
        }
        Ok(())
    }
}
