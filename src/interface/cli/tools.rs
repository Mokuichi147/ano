//! `ano tools`: the tools and MCP servers a user or environment may use.

use super::ToolsArgs;
use crate::{
    application::registry::ToolRegistry,
    config::AppConfig,
    domain::{mcp::McpTransport, plan::TASK_PLAN_NAME, tool::DELEGATE_TASK_NAME},
    harness::names::{
        GIT_COMMIT_PUSH_NAME, REVIEW_CHANGES_NAME, WEB_FETCH_NAME, WORKSPACE_CHECK_NAME,
        WORKSPACE_EXEC_NAME,
    },
};
use anyhow::Result;

pub(super) fn list_tools(
    config: &AppConfig,
    user_id: &str,
    args: &ToolsArgs,
    registry: &ToolRegistry,
) -> Result<()> {
    let mut policy = config.policy_for(user_id, &args.disabled_tools);
    let environment = args
        .environment
        .as_deref()
        .map(|name| config.environment_for(name))
        .transpose()?;
    if let Some(environment) = environment {
        policy = policy.with_restrictions(
            environment.allowed_tools.as_deref(),
            &environment.disabled_tools,
        );
    }
    println!("Local tools:");
    if !policy.is_disabled(TASK_PLAN_NAME) {
        println!("  task_plan - Read/update this run's task plan (always available)");
    }
    if !policy.is_disabled(DELEGATE_TASK_NAME) {
        println!(
            "  delegate_task - Hand a task to a sub-agent with a fresh context (always available)"
        );
    }
    if !policy.is_disabled(REVIEW_CHANGES_NAME) {
        println!(
            "  review_changes - Have a read-only reviewer in a fresh context review the uncommitted changes; git_commit_push needs it (always available)"
        );
    }
    for definition in registry.definitions(&policy) {
        let mut notes = Vec::new();
        if definition.name == WORKSPACE_EXEC_NAME {
            notes.push(match environment {
                Some(environment) if environment.allow_exec => "enabled by allow_exec",
                Some(_) => "unavailable: allow_exec is not set",
                None => "needs --allow-exec",
            });
        }
        if definition.name == WEB_FETCH_NAME {
            notes.push(match environment {
                Some(environment) if environment.allow_web => "enabled by allow_web",
                Some(_) => "unavailable: allow_web is not set",
                None => "needs --allow-web",
            });
        }
        if definition.name == WORKSPACE_CHECK_NAME {
            notes.push(match environment {
                Some(environment) if !environment.checks.is_empty() => {
                    "runs the environment's checks"
                }
                Some(_) => "unavailable: the environment has no checks",
                None => "needs an environment with checks",
            });
        }
        if definition.name == GIT_COMMIT_PUSH_NAME {
            notes.push(match environment {
                Some(environment) if environment.allow_writes => "enabled by allow_writes",
                Some(_) => "unavailable: allow_writes is not set",
                None => "needs --allow-writes",
            });
        }
        if definition.requires_approval {
            notes.push("requires approval");
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!(" [{}]", notes.join(", "))
        };
        println!("  {}{notes} - {}", definition.name, definition.description);
    }
    println!("\nMCP servers:");
    for server in &config.mcp_servers {
        if policy.is_mcp_server_disabled(&server.label) {
            println!("  {} (disabled)", server.label);
            continue;
        }
        let filtered = if server.transport == McpTransport::Responses {
            Some(
                server
                    .discoverable_tools(&policy)
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect::<Vec<_>>(),
            )
        } else {
            server.allowed_tools.as_ref().map(|names| {
                names
                    .iter()
                    .filter(|name| server.is_tool_allowed(&policy, name))
                    .cloned()
                    .collect::<Vec<_>>()
            })
        };
        let target = match server.transport {
            McpTransport::Responses => server
                .url
                .as_deref()
                .or(server.tunnel_id.as_deref())
                .unwrap_or("(invalid remote MCP configuration)")
                .to_string(),
            McpTransport::Stdio => format!(
                "stdio: {}",
                server.command.as_deref().unwrap_or("(missing command)")
            ),
            McpTransport::StreamableHttp => format!(
                "streamable_http: {}{}",
                server.url.as_deref().unwrap_or("(missing URL)"),
                if server.oauth { " (OAuth)" } else { "" }
            ),
        };
        println!("  {} ({:?}) -> {}", server.label, server.transport, target);
        if let Some(names) = filtered {
            if names.is_empty() {
                println!("    discoverable_tools: none (check catalog and policy)");
            } else {
                println!(
                    "    configured tools permitted by policy: {}",
                    names.join(", ")
                );
            }
        } else {
            println!(
                "    tool list not fetched (see `ano mcp tools {}`); user and environment policies apply at runtime",
                server.label
            );
        }
    }
    Ok(())
}
