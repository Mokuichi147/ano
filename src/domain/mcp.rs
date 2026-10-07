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
    /// `streamable_http` only: authorize with OAuth using the credentials
    /// saved by `ano mcp login <label>`.
    #[serde(default)]
    pub oauth: bool,
    /// Scopes requested at login. Defaults to the scopes the server advertises.
    pub oauth_scopes: Option<Vec<String>>,
    pub allowed_tools: Option<Vec<String>>,
    /// Tools of this server that are never used, applied after `allowed_tools`.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
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
    /// Why this server cannot be used, set when loading the config could not
    /// resolve one of its settings (such as an unset environment variable in
    /// `url`). Only this server is left out; the rest of ano keeps working.
    #[serde(skip)]
    pub unavailable: Option<String>,
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
        if self.oauth_scopes.is_some() && !self.oauth {
            bail!("MCP server '{label}' sets `oauth_scopes` without `oauth = true`");
        }
        if self.oauth {
            if self.transport != McpTransport::StreamableHttp {
                bail!("MCP server '{label}' can use `oauth` only with `transport = \"streamable_http\"`");
            }
            if self.authorization_env.is_some() {
                bail!("MCP server '{label}' must not set both `oauth` and `authorization_env`");
            }
        }

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

    /// Fail with the reason the server cannot be used, if loading the config
    /// recorded one.
    pub fn ensure_usable(&self) -> Result<()> {
        match &self.unavailable {
            Some(reason) => bail!("MCP server '{}' is unavailable: {reason}", self.label),
            None => Ok(()),
        }
    }

    /// Whether the server's own `allowed_tools` and `disabled_tools` permit
    /// `tool_name`, before any user policy.
    pub fn is_tool_enabled(&self, tool_name: &str) -> bool {
        self.allowed_tools
            .as_ref()
            .is_none_or(|names| names.iter().any(|name| name == tool_name))
            && !self.disabled_tools.iter().any(|name| name == tool_name)
    }

    /// Whether the model can find `tool_name` on this server, before any user
    /// policy. A Responses-managed server offers only the tools named in its
    /// `tool_catalog`, or in `allowed_tools` when it has no catalog.
    pub fn offers_tool(&self, tool_name: &str) -> bool {
        self.is_tool_enabled(tool_name)
            && (self.transport != McpTransport::Responses
                || match &self.tool_catalog {
                    Some(catalog) => catalog.iter().any(|tool| tool.name == tool_name),
                    None => self.allowed_tools.is_some(),
                })
    }

    /// Enable or disable tools by editing `allowed_tools` and
    /// `disabled_tools`. A server with an allowlist keeps using it, so tools
    /// the server adds later stay disabled; otherwise disabled tools are
    /// listed in `disabled_tools` and new tools are enabled.
    pub fn set_tools_enabled<'a>(
        &mut self,
        tool_names: impl IntoIterator<Item = &'a str>,
        enabled: bool,
    ) {
        for tool_name in tool_names {
            if enabled {
                self.disabled_tools.retain(|name| name != tool_name);
                // A Responses-managed server without a catalog offers only
                // the tools named in `allowed_tools`, so enabling one needs
                // an allowlist.
                let needs_allowlist =
                    self.transport == McpTransport::Responses && self.tool_catalog.is_none();
                if self.allowed_tools.is_none() && needs_allowlist {
                    self.allowed_tools = Some(Vec::new());
                }
                if let Some(allowed) = &mut self.allowed_tools {
                    if !allowed.iter().any(|name| name == tool_name) {
                        allowed.push(tool_name.to_string());
                    }
                }
            } else if let Some(allowed) = &mut self.allowed_tools {
                allowed.retain(|name| name != tool_name);
            } else if !self.disabled_tools.iter().any(|name| name == tool_name) {
                self.disabled_tools.push(tool_name.to_string());
            }
        }
    }

    pub fn is_tool_allowed(&self, policy: &UserPolicy, tool_name: &str) -> bool {
        self.is_tool_enabled(tool_name)
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

#[cfg(test)]
mod tests {
    use super::{McpServerConfig, McpTransport};

    fn server(extra: &str) -> McpServerConfig {
        let text = format!("label = 'files'\ntransport = 'stdio'\ncommand = 'node'\n{extra}");
        toml::from_str(&text).unwrap()
    }

    #[test]
    fn disabling_without_an_allowlist_uses_disabled_tools() {
        let mut server = server("");
        server.set_tools_enabled(["delete", "move"], false);
        assert_eq!(server.allowed_tools, None);
        assert_eq!(server.disabled_tools, ["delete", "move"]);
        assert!(!server.is_tool_enabled("delete"));
        assert!(server.is_tool_enabled("read"));

        server.set_tools_enabled(["delete"], true);
        assert_eq!(server.disabled_tools, ["move"]);
        assert!(server.offers_tool("delete"));
    }

    #[test]
    fn an_allowlist_is_kept_when_toggling() {
        let mut server = server("allowed_tools = ['read']\ndisabled_tools = ['write']");
        server.set_tools_enabled(["write", "list"], true);
        assert_eq!(
            server.allowed_tools.as_deref().unwrap(),
            ["read", "write", "list"]
        );
        assert!(server.disabled_tools.is_empty());

        server.set_tools_enabled(["read"], false);
        assert_eq!(server.allowed_tools.as_deref().unwrap(), ["write", "list"]);
        assert!(server.disabled_tools.is_empty());
        assert!(!server.offers_tool("read"));
    }

    #[test]
    fn responses_servers_offer_only_catalog_or_allowlisted_tools() {
        let mut remote = server("");
        remote.transport = McpTransport::Responses;
        remote.command = None;
        remote.url = Some("https://example.test/mcp".into());
        assert!(!remote.offers_tool("search"));

        remote.set_tools_enabled(["search"], true);
        assert_eq!(remote.allowed_tools.as_deref().unwrap(), ["search"]);
        assert!(remote.offers_tool("search"));
        assert!(!remote.offers_tool("delete"));

        let mut catalog: McpServerConfig = toml::from_str(
            "label = 'docs'\nurl = 'https://example.test/mcp'\ntool_catalog = [{ name = 'search' }]",
        )
        .unwrap();
        assert!(catalog.offers_tool("search"));
        assert!(!catalog.offers_tool("read"));
        catalog.set_tools_enabled(["search"], false);
        assert_eq!(catalog.disabled_tools, ["search"]);
        assert!(!catalog.offers_tool("search"));
    }
}
