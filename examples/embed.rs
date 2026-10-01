//! Embed ano in an application: register tools, share MCP connections, and
//! run a task in a workspace.
//!
//! ```sh
//! OPENAI_API_KEY=sk-... cargo run --example embed -- "List the files in this directory"
//! ```

use ano::{
    register_builtin_tools, Agent, AgentSettings, DenyApproval, InputPart, McpGateway, McpPool,
    OpenAiClient, ReviewGate, RunRequest, ToolContext, ToolDefinition, ToolRegistry, UserPolicy,
    DEFAULT_INSTRUCTIONS,
};
use anyhow::Result;
use serde_json::json;
use std::sync::Arc;

fn register_tools(registry: &ToolRegistry) -> Result<()> {
    // A plain tool receives only its JSON arguments.
    registry.register(
        ToolDefinition::new(
            "lookup_customer",
            "Look up a customer by id.",
            json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false
            }),
        ),
        |arguments| async move {
            let id = arguments["id"].as_str().unwrap_or_default();
            Ok(json!({"id": id, "status": "example"}))
        },
    )?;
    // A contextual tool also receives the selected user and environment.
    registry.register_contextual(
        ToolDefinition::new(
            "environment_info",
            "Return the selected environment.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        |_arguments, context: ToolContext| async move {
            Ok(json!({"environment": context.environment}))
        },
    )?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "List the files in the workspace.".into());

    let registry = ToolRegistry::new();
    register_builtin_tools(&registry)?;
    register_tools(&registry)?;

    // One pool per process; every agent that shares it reuses MCP connections.
    let mcp: Arc<dyn McpGateway> = Arc::new(McpPool::new(Vec::new()));
    let agent = Agent::new(
        OpenAiClient::from_env()?,
        // The instructions of the built-in tools, as `ano` uses by default.
        AgentSettings {
            instructions: DEFAULT_INSTRUCTIONS.to_string(),
            ..AgentSettings::default()
        },
        Arc::clone(&mcp),
        registry,
        UserPolicy::default(),
        Arc::new(DenyApproval),
    )
    // The built-in git_commit_push commits only reviewed files; the gate
    // offers review_changes and checks commits against its reviews.
    .with_extension(Arc::new(ReviewGate::new()))
    .with_event_listener(Arc::new(|event| eprintln!("{event:?}")));

    let request = RunRequest {
        raw_input: None,
        input: vec![InputPart::Text(prompt)],
        context: ToolContext {
            workspace: Some(std::env::current_dir()?),
            ..ToolContext::default()
        },
        goal: None,
    };
    let result = agent.run(request).await;
    mcp.shutdown().await;

    let result = result?;
    println!("{}\n\noutcome: {:?}", result.text, result.outcome);
    Ok(())
}
