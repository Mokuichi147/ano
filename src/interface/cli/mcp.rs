//! `ano mcp`: OAuth login and logout for directly connected MCP servers, and
//! listing and enabling the tools of configured MCP servers.

use super::prompt::{answered, require_terminal};
use crate::{
    application::ports::McpGateway,
    config::{save_mcp_tool_filters, AppConfig},
    domain::{
        mcp::{McpServerConfig, McpToolCatalog, McpTransport},
        policy::UserPolicy,
    },
    infrastructure::{
        mcp::{list_server_tools, redact_urls, McpPool},
        mcp_oauth::OAuthStore,
    },
};
use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use inquire::{list_option::ListOption, MultiSelect};
use std::{
    fmt,
    io::IsTerminal,
    path::Path,
    process::{Command, Stdio},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Args)]
pub(super) struct McpArgs {
    #[command(subcommand)]
    command: McpCommand,
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Authorize ano to use an MCP server configured with `oauth = true`.
    Login {
        label: String,
        #[arg(long, help = "Print the authorization URL without opening a browser")]
        no_browser: bool,
    },
    /// Delete the saved OAuth credentials of an MCP server.
    Logout { label: String },
    /// Connect to MCP servers and list every tool they offer, marking the
    /// tools the config file enables.
    Tools {
        /// Server to list. Omit to list every configured server.
        label: Option<String>,
    },
    /// Choose the enabled tools of an MCP server from a checklist and save
    /// the choice to the config file.
    Edit { label: String },
    /// Enable tools of an MCP server in the config file.
    Enable(ToggleArgs),
    /// Disable tools of an MCP server in the config file.
    Disable(ToggleArgs),
}

#[derive(Debug, Args)]
struct ToggleArgs {
    label: String,
    #[arg(required = true, value_name = "TOOL")]
    tools: Vec<String>,
    #[arg(
        long,
        help = "Save without connecting to the server to check the tool names"
    )]
    no_verify: bool,
}

pub(super) async fn run(
    config: &AppConfig,
    config_path: &Path,
    user_id: &str,
    args: McpArgs,
) -> Result<()> {
    let store = OAuthStore::default_location();
    match args.command {
        McpCommand::Login { label, no_browser } => {
            let server = find_server(config, &label)?;
            store
                .login(server, |url| {
                    eprintln!(
                        "Open this URL to authorize ano for MCP server '{label}':\n\n  {url}\n"
                    );
                    if !no_browser {
                        open_browser(url);
                    }
                    eprintln!("Waiting for the authorization to finish...");
                })
                .await?;
            // Connect once so a login that the server does not accept is
            // reported now rather than at the next run.
            let pool = McpPool::new(vec![server.clone()]).with_oauth_store(store);
            let connected = pool.connect(&UserPolicy::default()).await;
            pool.shutdown().await;
            let tools = connected?.first().map_or(0, |server| server.tools().len());
            println!("Logged in to MCP server '{label}' ({tools} tools available).");
            Ok(())
        }
        McpCommand::Logout { label } => {
            let server = find_server(config, &label)?;
            if store.remove(server)? {
                println!("Deleted the saved OAuth credentials of MCP server '{label}'.");
            } else {
                println!("MCP server '{label}' has no saved OAuth credentials.");
            }
            Ok(())
        }
        McpCommand::Tools { label } => {
            let policy = config.policy_for(user_id, &[]);
            let servers = match &label {
                Some(label) => vec![find_server(config, label)?],
                None if config.mcp_servers.is_empty() => {
                    bail!("no MCP servers are configured in {}", config_path.display())
                }
                None => config.mcp_servers.iter().collect(),
            };
            let mut failed = 0;
            for (index, server) in servers.into_iter().enumerate() {
                if index > 0 {
                    println!();
                }
                match list_server_tools(server, &store).await {
                    Ok(tools) => print_tools(server, &tools, &policy, user_id),
                    Err(error) => {
                        failed += 1;
                        println!("{}", server_heading(server));
                        println!("  error: {}", redact_urls(&format!("{error:#}")));
                    }
                }
            }
            if failed > 0 {
                bail!("failed to list the tools of {failed} MCP server(s)");
            }
            Ok(())
        }
        McpCommand::Edit { label } => {
            let mut server = find_server(config, &label)?.clone();
            require_terminal("ano mcp edit", &["ano mcp enable", "ano mcp disable"])?;
            let tools = list_server_tools(&server, &store).await?;
            if tools.is_empty() {
                println!("MCP server '{label}' offers no tools.");
                return Ok(());
            }
            let Some(selected) = choose_tools(&server, &tools)? else {
                println!("Cancelled; the config file was not changed.");
                return Ok(());
            };
            let (enable, disable): (Vec<_>, Vec<_>) = tools
                .iter()
                .map(|tool| tool.name.as_str())
                .filter(|name| selected.contains(name) != server.offers_tool(name))
                .partition(|name| selected.contains(name));
            if enable.is_empty() && disable.is_empty() {
                println!("No changes.");
                return Ok(());
            }
            // Enable first: disabling then only removes names from lists.
            server.set_tools_enabled(enable.iter().copied(), true);
            server.set_tools_enabled(disable.iter().copied(), false);
            save_mcp_tool_filters(config_path, &server)?;
            for (heading, names) in [("Enabled", &enable), ("Disabled", &disable)] {
                if !names.is_empty() {
                    println!("{heading}: {}", names.join(", "));
                }
            }
            warn_unoffered(&server, &enable);
            println!("Saved to {}.", config_path.display());
            Ok(())
        }
        McpCommand::Enable(args) => toggle(config, config_path, &store, args, true).await,
        McpCommand::Disable(args) => toggle(config, config_path, &store, args, false).await,
    }
}

async fn toggle(
    config: &AppConfig,
    config_path: &Path,
    store: &OAuthStore,
    args: ToggleArgs,
    enabled: bool,
) -> Result<()> {
    let mut server = find_server(config, &args.label)?.clone();
    let label = &args.label;
    if !args.no_verify {
        let tools = list_server_tools(&server, store)
            .await
            .context("failed to check the tool names (use --no-verify to skip the check)")?;
        let unknown: Vec<&str> = args
            .tools
            .iter()
            .filter(|name| !tools.iter().any(|tool| &tool.name == *name))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            bail!(
                "MCP server '{label}' does not offer {}; run `ano mcp tools {label}` to see its tools",
                unknown.join(", ")
            );
        }
    }
    let changed: Vec<&str> = args
        .tools
        .iter()
        .map(String::as_str)
        .filter(|name| server.offers_tool(name) != enabled)
        .collect();
    let state = if enabled { "enabled" } else { "disabled" };
    if changed.is_empty() {
        println!("Already {state}: {}", args.tools.join(", "));
        return Ok(());
    }
    server.set_tools_enabled(changed.iter().copied(), enabled);
    save_mcp_tool_filters(config_path, &server)?;
    println!(
        "{} for MCP server '{label}': {}",
        if enabled { "Enabled" } else { "Disabled" },
        changed.join(", ")
    );
    if enabled {
        warn_unoffered(&server, &changed);
    }
    println!("Saved to {}.", config_path.display());
    Ok(())
}

/// A Responses-managed server with a `tool_catalog` offers only the tools in
/// the catalog, which `ano mcp` does not edit.
fn warn_unoffered(server: &McpServerConfig, enabled: &[&str]) {
    let missing: Vec<&str> = enabled
        .iter()
        .copied()
        .filter(|name| !server.offers_tool(name))
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "warning: {} not in the tool_catalog of MCP server '{}'; add them to the catalog so the model can find them",
            missing.join(", "),
            server.label
        );
    }
}

fn server_heading(server: &McpServerConfig) -> String {
    let transport = match server.transport {
        McpTransport::Responses => "responses",
        McpTransport::Stdio => "stdio",
        McpTransport::StreamableHttp if server.oauth => "streamable_http, OAuth",
        McpTransport::StreamableHttp => "streamable_http",
    };
    format!("{} ({transport})", server.label)
}

fn print_tools(
    server: &McpServerConfig,
    tools: &[McpToolCatalog],
    policy: &UserPolicy,
    user_id: &str,
) {
    let enabled = tools
        .iter()
        .filter(|tool| server.offers_tool(&tool.name))
        .count();
    println!(
        "{}: {} tools, {enabled} enabled",
        server_heading(server),
        tools.len()
    );
    let name_width = tools
        .iter()
        .map(|tool| tool.name.width())
        .max()
        .unwrap_or(0)
        .min(40);
    let line_width = std::io::stdout()
        .is_terminal()
        .then(|| usize::from(termimad::terminal_size().0));
    for tool in tools {
        let mark = if server.offers_tool(&tool.name) {
            "[x]"
        } else {
            "[ ]"
        };
        let mut line = format!(
            "  {mark} {}{}",
            tool.name,
            " ".repeat(name_width.saturating_sub(tool.name.width()))
        );
        if server.offers_tool(&tool.name) && !server.is_tool_allowed(policy, &tool.name) {
            line.push_str(&format!("  (disabled for user '{user_id}')"));
        }
        if let Some(description) = first_line(tool.description.as_deref()) {
            line.push_str("  ");
            line.push_str(description);
        }
        match line_width {
            Some(width) => println!("{}", truncate(&line, width)),
            None => println!("{line}"),
        }
    }

    let unknown: Vec<&str> = server
        .allowed_tools
        .iter()
        .flatten()
        .chain(&server.disabled_tools)
        .map(String::as_str)
        .filter(|name| !tools.iter().any(|tool| tool.name == *name))
        .collect();
    if !unknown.is_empty() {
        println!(
            "  Names in the config that the server does not offer: {}",
            unknown.join(", ")
        );
    }
    if server.transport == McpTransport::Responses
        && server.tool_catalog.is_none()
        && server.allowed_tools.is_none()
    {
        println!(
            "  No tool is enabled until allowed_tools or tool_catalog names it; use `ano mcp enable` or `ano mcp edit`."
        );
    }
}

/// Show a checklist of `tools` with the enabled ones checked. Returns the
/// checked tool names, or `None` when the user cancels.
fn choose_tools<'a>(
    server: &McpServerConfig,
    tools: &'a [McpToolCatalog],
) -> Result<Option<Vec<&'a str>>> {
    let defaults: Vec<usize> = tools
        .iter()
        .enumerate()
        .filter(|(_, tool)| server.offers_tool(&tool.name))
        .map(|(index, _)| index)
        .collect();
    let width = usize::from(termimad::terminal_size().0).saturating_sub(8);
    let choices = tools
        .iter()
        .map(|tool| ToolChoice { tool, width })
        .collect();
    let message = format!("Tools enabled for MCP server '{}'", server.label);
    // The default formatter repeats every checked row after the answer.
    let summary =
        |checked: &[ListOption<&ToolChoice>]| format!("{} of {} tools", checked.len(), tools.len());
    let answer = MultiSelect::new(&message, choices)
        .with_default(&defaults)
        .with_formatter(&summary)
        .with_page_size(15)
        .with_help_message(
            "↑↓ move, space toggle, → all, ← none, type to filter, enter save, esc cancel",
        )
        .prompt();
    Ok(answered(answer)?.map(|selected| {
        selected
            .into_iter()
            .map(|choice| choice.tool.name.as_str())
            .collect()
    }))
}

/// One checklist row: the tool name and the start of its description.
struct ToolChoice<'a> {
    tool: &'a McpToolCatalog,
    width: usize,
}

impl fmt::Display for ToolChoice<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let line = match first_line(self.tool.description.as_deref()) {
            Some(description) => format!("{} - {description}", self.tool.name),
            None => self.tool.name.clone(),
        };
        formatter.write_str(&truncate(&line, self.width))
    }
}

fn first_line(text: Option<&str>) -> Option<&str> {
    text.and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
}

/// Cut `text` to at most `width` terminal columns, marking the cut with `…`.
fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut result = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width + 1 > width {
            break;
        }
        used += character_width;
        result.push(character);
    }
    result.push('…');
    result
}

fn find_server<'a>(config: &'a AppConfig, label: &str) -> Result<&'a McpServerConfig> {
    config
        .mcp_servers
        .iter()
        .find(|server| server.label == label)
        .with_context(|| format!("unknown MCP server '{label}'"))
}

/// Best effort: the URL is also printed for when no browser can be opened.
fn open_browser(url: &str) {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        Command::new("explorer")
    } else {
        Command::new("xdg-open")
    };
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().ok();
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncates_by_terminal_columns() {
        assert_eq!(truncate("search_works", 20), "search_works");
        assert_eq!(truncate("search_works", 7), "search…");
        assert_eq!(truncate("作品を検索します", 7), "作品を…");
    }
}
