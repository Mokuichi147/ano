//! `workspace_check`: run a validation command fixed by the environment.

use super::process::run_bounded;
use crate::{
    application::registry::ToolRegistry,
    domain::tool::{ToolContext, ToolDefinition},
    harness::names::WORKSPACE_CHECK_NAME,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;

pub(super) fn register(registry: &ToolRegistry) -> Result<()> {
    registry.register_contextual(ToolDefinition::new(
        WORKSPACE_CHECK_NAME,
        "Run a configured build, test, or validation check in the workspace. Use name=null to list available checks, then run an exact name. The program and arguments are fixed by the environment. Inspect success, exit_code, stdout and stderr; never claim validation passed when it failed.",
        json!({"type":"object","properties":{"name":{"type":["string","null"]}},"required":["name"],"additionalProperties":false}),
    )
    // Without configured checks there is nothing to run, and a model offered
    // the tool calls it with commands as check names.
    .available_when(|context| !context.checks.is_empty())
    // A check has its own deadline and keeps its partial output when it
    // fires; leave it time to report.
    .with_deadline(|arguments, context| {
        let check = context.checks.get(arguments["name"].as_str()?)?;
        Some(check.timeout_secs.saturating_add(5))
    }), |arguments, context| async move { run_check(arguments, &context).await })
}

async fn run_check(arguments: Value, context: &ToolContext) -> Result<Value> {
    let name = arguments
        .get("name")
        .context("workspace_check.name is required (null lists checks)")?;
    if name.is_null() {
        return Ok(
            json!({"checks":context.checks.iter().map(|(name, check)| json!({
            "name":name,"description":check.description,"timeout_secs":check.timeout_secs
        })).collect::<Vec<_>>() }),
        );
    }
    let name = name
        .as_str()
        .context("workspace_check.name must be a string or null")?;
    let check = context.checks.get(name).with_context(|| format!("check '{name}' is not configured for this environment; list available checks with name=null"))?;
    check.validate()?;
    let workspace = tokio::fs::canonicalize(
        context
            .workspace
            .as_ref()
            .context("no workspace is configured")?,
    )
    .await?;
    let mut command = Command::new(&check.program);
    command.args(&check.args).current_dir(workspace);
    let output = run_bounded(command, Duration::from_secs(check.timeout_secs))
        .await
        .with_context(|| format!("failed to start configured check '{name}'"))?;
    let mut result = json!({
        "name": name,
        "success": output.success(),
        "exit_code": output.exit_code(),
        "timed_out": output.timed_out(),
        "stdout": output.stdout.text(),
        "stderr": output.stderr.text(),
        "stdout_truncated": output.stdout.truncated(),
        "stderr_truncated": output.stderr.truncated(),
    });
    if !output.success() {
        result["error"] = json!(if output.timed_out() {
            "check_timed_out"
        } else {
            "check_failed"
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::environment::CheckConfig;

    #[test]
    fn checks_are_offered_only_when_the_environment_has_some_with_their_deadline() {
        let registry = ToolRegistry::new();
        register(&registry).unwrap();
        let definition = registry.definition(WORKSPACE_CHECK_NAME).unwrap();
        let mut context = ToolContext::default();
        assert!(!definition.can_run(&context));
        context.checks.insert(
            "test".into(),
            CheckConfig {
                program: "cargo".into(),
                args: vec!["test".into()],
                description: String::new(),
                timeout_secs: 90,
            },
        );
        assert!(definition.can_run(&context));
        // The check's own deadline fires first and keeps its output.
        assert_eq!(
            definition.deadline_secs(&json!({"name": "test"}), &context),
            Some(95)
        );
        assert_eq!(
            definition.deadline_secs(&json!({"name": null}), &context),
            None
        );
    }

    #[tokio::test]
    async fn only_explicitly_configured_checks_can_run() {
        let workspace = tempfile::tempdir().unwrap();
        let mut context = ToolContext {
            workspace: Some(workspace.path().to_owned()),
            ..Default::default()
        };
        assert!(run_check(json!({"name":"unknown"}), &context)
            .await
            .is_err());
        context.checks.insert(
            "list_tests".into(),
            CheckConfig {
                program: std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                args: vec!["--list".into()],
                description: "List test cases".into(),
                timeout_secs: 5,
            },
        );
        let listed = run_check(json!({"name":null}), &context).await.unwrap();
        assert_eq!(listed["checks"][0]["name"], "list_tests");
        let result = run_check(json!({"name":"list_tests"}), &context)
            .await
            .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["exit_code"], 0);
        assert!(result["stdout"]
            .as_str()
            .unwrap()
            .contains("only_explicitly_configured_checks_can_run"));
        context.checks.get_mut("list_tests").unwrap().args = vec!["--invalid-test-option".into()];
        let result = run_check(json!({"name":"list_tests"}), &context)
            .await
            .unwrap();
        assert_eq!(result["success"], false);
        assert_eq!(result["error"], "check_failed");
        assert!(!result["stderr"].as_str().unwrap().is_empty());
    }

    #[test]
    #[ignore = "subprocess fixture for timeout test"]
    fn slow_check_fixture() {
        use std::io::Write;
        print!("started slow check");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    #[tokio::test]
    async fn timeout_stops_waiting_and_keeps_partial_output() {
        let workspace = tempfile::tempdir().unwrap();
        let mut context = ToolContext {
            workspace: Some(workspace.path().to_owned()),
            ..Default::default()
        };
        context.checks.insert(
            "slow".into(),
            CheckConfig {
                program: std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                args: vec![
                    "--exact".into(),
                    "infrastructure::tools::checks::tests::slow_check_fixture".into(),
                    "--ignored".into(),
                    "--nocapture".into(),
                ],
                description: String::new(),
                timeout_secs: 1,
            },
        );
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_check(json!({"name":"slow"}), &context),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["success"], false);
        assert_eq!(result["error"], "check_timed_out");
        assert!(result["stdout"]
            .as_str()
            .unwrap()
            .contains("started slow check"));
    }
}
