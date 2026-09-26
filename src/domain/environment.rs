//! Named execution environments and their trusted validation checks.

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
    pub auto_approve_mcp: bool,
    /// Explicitly trusted validation programs. Arguments are fixed by config.
    pub checks: BTreeMap<String, CheckConfig>,
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
