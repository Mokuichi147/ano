//! `workspace_exec`: run a shell command in the workspace.
//!
//! Only available when the environment sets `allow_exec`, and every call
//! goes through the run's approval handler first. Commands run with ano's OS
//! permissions and are not sandboxed; the workspace is only the working
//! directory.

use super::{
    args::{optional_integer, optional_string},
    paths::{existing_workspace_path, relative_path},
    process::run_bounded,
};
use crate::{
    application::registry::ToolRegistry,
    domain::tool::{ToolContext, ToolDefinition},
    harness::names::{EXEC_DEFAULT_TIMEOUT_SECS, EXEC_MAX_TIMEOUT_SECS, WORKSPACE_EXEC_NAME},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;

/// Environment variables whose names contain one of these words are not
/// passed to commands, so a command cannot print ano's API keys or tokens.
const SECRET_MARKERS: &[&str] = &["KEY", "SECRET", "TOKEN", "PASSWORD", "PASSWD", "CREDENTIAL"];

pub(super) fn register(registry: &ToolRegistry) -> Result<()> {
    let mut definition = ToolDefinition::new(
        WORKSPACE_EXEC_NAME,
        format!("Run a shell command in the workspace (sh -c on Unix, cmd /C on Windows) and return its exit code and output. Use it for commands no other tool covers, such as git, builds, tests, or project scripts; prefer workspace_read/search/find/edit for files. Each command needs approval and may be denied. stdin is closed, so avoid interactive commands; background processes are stopped when the command ends. Output keeps the beginning and the end when long. timeout_secs defaults to {EXEC_DEFAULT_TIMEOUT_SECS} (max {EXEC_MAX_TIMEOUT_SECS})."),
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "minLength": 1, "description": "Shell command line"},
                "cwd": {"type": "string", "description": "Relative working directory inside the workspace; defaults to ."},
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": EXEC_MAX_TIMEOUT_SECS}
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    )
    .with_approval()
    .targeted()
    .available_when(|context| context.allow_exec)
    // The command has its own deadline and keeps its partial output when it
    // fires; leave it time to report.
    .with_deadline(|arguments, _| {
        Some(
            arguments["timeout_secs"]
                .as_u64()
                .unwrap_or(EXEC_DEFAULT_TIMEOUT_SECS)
                .min(EXEC_MAX_TIMEOUT_SECS)
                .saturating_add(5),
        )
    });
    // cwd and timeout_secs are optional.
    definition.strict = false;
    registry.register_contextual(definition, |arguments, context| async move {
        workspace_exec(arguments, &context).await
    })
}

async fn workspace_exec(arguments: Value, context: &ToolContext) -> Result<Value> {
    if !context.allow_exec {
        bail!("command execution is not enabled for this environment (allow_exec)");
    }
    let command_line = arguments
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.trim().is_empty())
        .context("workspace_exec.command must be a non-empty string")?;
    let cwd = relative_path(optional_string(&arguments, "cwd", ".")?)?;
    let timeout_secs = optional_integer(
        &arguments,
        "timeout_secs",
        EXEC_DEFAULT_TIMEOUT_SECS,
        1,
        EXEC_MAX_TIMEOUT_SECS,
    )?;
    let directory = existing_workspace_path(context, &cwd).await?;
    if !tokio::fs::metadata(&directory).await?.is_dir() {
        bail!("cwd is not a directory: {}", cwd.display());
    }

    let mut command = shell(command_line);
    command.current_dir(&directory);
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if SECRET_MARKERS.iter().any(|marker| upper.contains(marker)) {
            command.env_remove(&name);
        }
    }
    let output = run_bounded(command, Duration::from_secs(timeout_secs))
        .await
        .context("failed to start the shell")?;
    let mut result = json!({
        "command": command_line,
        "cwd": cwd,
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
            "command_timed_out"
        } else {
            "command_failed"
        });
    }
    Ok(result)
}

#[cfg(unix)]
fn shell(command_line: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(command_line);
    command
}

#[cfg(windows)]
fn shell(command_line: &str) -> Command {
    let mut command = Command::new("cmd");
    command.arg("/C").raw_arg(command_line);
    command
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn context(workspace: &std::path::Path, allow_exec: bool) -> ToolContext {
        ToolContext {
            workspace: Some(workspace.to_path_buf()),
            allow_exec,
            ..ToolContext::default()
        }
    }

    #[tokio::test]
    async fn runs_in_the_workspace_and_reports_failures() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("sub")).unwrap();
        let context = context(workspace.path(), true);

        let result = workspace_exec(
            json!({"command": "pwd; echo err >&2", "cwd": "sub"}),
            &context,
        )
        .await
        .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["exit_code"], 0);
        let pwd = std::fs::canonicalize(workspace.path().join("sub")).unwrap();
        assert_eq!(
            result["stdout"].as_str().unwrap().trim(),
            pwd.to_str().unwrap()
        );
        assert_eq!(result["stderr"].as_str().unwrap().trim(), "err");

        let failed = workspace_exec(json!({"command": "exit 3"}), &context)
            .await
            .unwrap();
        assert_eq!(failed["success"], false);
        assert_eq!(failed["exit_code"], 3);
        assert_eq!(failed["error"], "command_failed");

        for cwd in ["..", "/tmp", "missing"] {
            assert!(
                workspace_exec(json!({"command": "true", "cwd": cwd}), &context)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn requires_allow_exec() {
        let workspace = tempfile::tempdir().unwrap();
        let error = workspace_exec(
            json!({"command": "true"}),
            &context(workspace.path(), false),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("allow_exec"));
    }

    #[tokio::test]
    async fn secrets_are_not_passed_to_commands() {
        let workspace = tempfile::tempdir().unwrap();
        // The variable is set by this test only, and is read back by the
        // child process through its own environment.
        std::env::set_var("ANO_TEST_EXEC_API_KEY", "must-not-leak");
        let result = workspace_exec(
            json!({"command": "echo \"[$ANO_TEST_EXEC_API_KEY]\"; echo \"$PATH\""}),
            &context(workspace.path(), true),
        )
        .await
        .unwrap();
        let stdout = result["stdout"].as_str().unwrap();
        assert!(stdout.starts_with("[]\n"), "{stdout}");
        assert!(stdout.lines().nth(1).is_some_and(|path| !path.is_empty()));
    }

    #[tokio::test]
    async fn timeout_stops_background_processes_and_keeps_output() {
        let workspace = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let result = workspace_exec(
            json!({"command": "echo started; sleep 30 & sleep 30", "timeout_secs": 1}),
            &context(workspace.path(), true),
        )
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["error"], "command_timed_out");
        assert_eq!(result["stdout"].as_str().unwrap().trim(), "started");

        // A finished command does not wait for what it left running.
        let started = Instant::now();
        let result = workspace_exec(
            json!({"command": "sleep 30 & echo done"}),
            &context(workspace.path(), true),
        )
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(result["success"], true);
        assert_eq!(result["stdout"].as_str().unwrap().trim(), "done");
    }
}
