//! Built-in local tools: `echo`, `unix_time`, workspace file access and
//! management, configured validation checks, shell commands, git commits and
//! pushes, and web pages.

mod args;
mod checks;
mod exec;
mod git;
mod glob;
mod manage;
pub mod names;
mod paths;
mod process;
mod walk;
mod web;
mod workspace;

use crate::{application::registry::ToolRegistry, domain::tool::ToolDefinition};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// Register the built-in tools into `registry`. `git_commit_push` commits
/// only files in the state of a review, which `harness::review::ReviewGate`
/// records and hands to it: give every agent that uses these tools a gate.
pub fn register_builtin_tools(registry: &ToolRegistry) -> Result<()> {
    checks::register(registry)?;
    exec::register(registry)?;
    web::register(registry)?;
    registry.register(
        non_strict_definition(
            "echo",
            "Return the supplied JSON value unchanged. Useful for testing tool wiring.",
            json!({
                "type": "object",
                "properties": {"value": {}},
                "required": ["value"],
                "additionalProperties": false
            }),
        ),
        |arguments| async move {
            arguments
                .get("value")
                .cloned()
                .context("echo.value is required")
        },
    )?;
    registry.register(
        ToolDefinition::new(
            "unix_time",
            "Return the current Unix timestamp.",
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        ),
        |_arguments| async move {
            let seconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock is before Unix epoch")?
                .as_secs();
            Ok(json!({"unix_seconds": seconds}))
        },
    )?;
    workspace::register(registry)
}

// Optional arguments and echo's arbitrary JSON value are intentionally not
// strict Responses schemas. Runtime handlers validate the optional arguments.
fn non_strict_definition(name: &str, description: &str, parameters: Value) -> ToolDefinition {
    let mut definition = ToolDefinition::new(name, description, parameters);
    definition.strict = false;
    definition
}
