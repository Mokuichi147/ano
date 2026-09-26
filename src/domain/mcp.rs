//! MCP server configuration and the rules that decide which tools a user may reach.

use crate::domain::policy::UserPolicy;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::{collections::HashMap, path::PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolCatalog {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    /// Let the Responses API provider connect to the remote MCP server.
    #[default]
    Responses,
    /// Launch a local MCP server process and communicate over stdio.
    Stdio,
    /// Connect directly to an MCP Streamable HTTP endpoint.
    StreamableHttp,
}

/// Whether an MCP tool call must be approved before it runs.
///
/// The default is `always` so an omitted setting is fail-safe; use `never`
/// only for a fully automatic trusted server.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpApprovalMode {
    #[default]
    Always,
    Never,
}

impl McpApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub label: String,
    #[serde(default)]
    pub transport: McpTransport,
    pub url: Option<String>,
    pub tunnel_id: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Child process variable name -> name of an environment variable in ano.
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    pub description: Option<String>,
    pub authorization_env: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    /// Optional lightweight metadata used by lazy tool discovery. When this
    /// is omitted, names from `allowed_tools` are used without descriptions.
    pub tool_catalog: Option<Vec<McpToolCatalog>>,
    #[serde(default)]
    pub require_approval: McpApprovalMode,
    /// Direct transports only: keep one connection (and one stdio process)
    /// shared by every run. Set to `false` for a stateful server that must
    /// not share state between tasks; it then connects once per run.
    #[serde(default = "default_reuse_connection")]
    pub reuse_connection: bool,
}

fn default_reuse_connection() -> bool {
    true
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<()> {
        let label = &self.label;
        if label.is_empty()
            || !label.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
        {
            bail!("MCP server label '{label}' must be non-empty and use only ASCII letters, digits, '_' or '-'");
        }
        let has_stdio_settings = self.command.is_some()
            || !self.args.is_empty()
            || self.cwd.is_some()
            || !self.env_vars.is_empty();

        match self.transport {
            McpTransport::Responses => {
                if self.url.is_some() == self.tunnel_id.is_some() {
                    bail!("MCP server '{label}' must set exactly one of `url` or `tunnel_id`");
                }
                if has_stdio_settings {
                    bail!("MCP server '{label}' uses the responses transport and cannot use `command`, `args`, `cwd`, or `env_vars`");
                }
                if !self.reuse_connection {
                    bail!("MCP server '{label}' uses the responses transport, which ano does not connect to; remove `reuse_connection`");
                }
            }
            McpTransport::Stdio => {
                if self
                    .command
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
                {
                    bail!("stdio MCP server '{label}' must set `command`");
                }
                if self.url.is_some() || self.tunnel_id.is_some() {
                    bail!("stdio MCP server '{label}' must not set `url` or `tunnel_id`");
                }
                if self.authorization_env.is_some() {
                    bail!("stdio MCP server '{label}' should pass credentials using `env_vars`, not `authorization_env`");
                }
            }
            McpTransport::StreamableHttp => {
                if self.url.as_deref().unwrap_or_default().trim().is_empty() {
                    bail!("streamable_http MCP server '{label}' must set `url`");
                }
                if self.tunnel_id.is_some() || has_stdio_settings {
                    bail!("streamable_http MCP server '{label}' must set `url` only; `tunnel_id`, `command`, `args`, `cwd`, and `env_vars` are not supported");
                }
            }
        }
        Ok(())
    }

    pub fn is_tool_allowed(&self, policy: &UserPolicy, tool_name: &str) -> bool {
        self.allowed_tools
            .as_ref()
            .map(|names| names.iter().any(|name| name == tool_name))
            .unwrap_or(true)
            && !policy.is_mcp_tool_disabled(&self.label, tool_name)
            && policy.is_mcp_tool_allowed(&self.label, tool_name)
    }

    pub fn requires_approval(&self) -> bool {
        self.require_approval == McpApprovalMode::Always
    }

    pub fn discoverable_tools(&self, policy: &UserPolicy) -> Vec<McpToolCatalog> {
        if policy.is_mcp_server_disabled(&self.label) {
            return Vec::new();
        }

        let candidates = self.tool_catalog.clone().unwrap_or_else(|| {
            self.allowed_tools
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|name| McpToolCatalog {
                    name,
                    description: None,
                })
                .collect()
        });

        candidates
            .into_iter()
            .filter(|candidate| self.is_tool_allowed(policy, &candidate.name))
            .collect()
    }
}
