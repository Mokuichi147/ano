//! `ano mcp`: OAuth login and logout for directly connected MCP servers.

use crate::{
    application::ports::McpGateway,
    config::AppConfig,
    domain::{mcp::McpServerConfig, policy::UserPolicy},
    infrastructure::{mcp::McpPool, mcp_oauth::OAuthStore},
};
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::process::{Command, Stdio};

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
}

pub(super) async fn run(config: &AppConfig, args: McpArgs) -> Result<()> {
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
    }
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
