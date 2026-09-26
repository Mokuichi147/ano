//! Built-in local tools: `echo`, `unix_time`, workspace file access and
//! management, configured validation checks, and shell commands.

mod checks;
mod exec;
mod glob;
mod manage;
mod process;
mod walk;
mod workspace;

use crate::{application::registry::ToolRegistry, domain::tool::ToolDefinition};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

/// Register the built-in tools into `registry`.
pub fn register_builtin_tools(registry: &ToolRegistry) -> Result<()> {
    checks::register(registry)?;
    exec::register(registry)?;
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
