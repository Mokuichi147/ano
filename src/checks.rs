use crate::{ToolContext, ToolDefinition, ToolRegistry};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub(crate) fn register_workspace_check(registry: &ToolRegistry) -> Result<()> {
    registry.register_contextual(ToolDefinition::new(
        "workspace_check",
        "Run a configured build, test, or validation check in the workspace. Use name=null to list available checks, then run an exact name. The program and arguments are fixed by the environment. Inspect success, exit_code, stdout and stderr; never claim validation passed when it failed.",
        json!({"type":"object","properties":{"name":{"type":["string","null"]}},"required":["name"],"additionalProperties":false}),
    ), |arguments, context| async move { run_check(arguments, &context).await })
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
    command
        .args(&check.args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start configured check '{name}'"))?;
    let stdout = child.stdout.take().context("check stdout unavailable")?;
    let stderr = child.stderr.take().context("check stderr unavailable")?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_total = 0;
    let mut err_total = 0;
    let result = tokio::time::timeout(Duration::from_secs(check.timeout_secs), async {
        tokio::try_join!(
            child.wait(),
            capture(stdout, &mut out, &mut out_total),
            capture(stderr, &mut err, &mut err_total)
        )
    })
    .await;
    let (status, timed_out) = match result {
        Ok(Ok((status, (), ()))) => (Some(status), false),
        Ok(Err(error)) => {
            child.kill().await.ok();
            return Err(error).context("failed to capture check output");
        }
        Err(_) => {
            child.kill().await.ok();
            (None, true)
        }
    };
    let success = status.is_some_and(|status| status.success());
    let mut output = json!({"name":name,"success":success,"exit_code":status.and_then(|status| status.code()),
        "timed_out":timed_out, "stdout":String::from_utf8_lossy(&out),"stderr":String::from_utf8_lossy(&err),
        "stdout_truncated":out_total > MAX_OUTPUT_BYTES,"stderr_truncated":err_total > MAX_OUTPUT_BYTES});
    if !success {
        output["error"] = json!(if timed_out {
            "check_timed_out"
        } else {
            "check_failed"
        });
    }
    Ok(output)
}

async fn capture(
    mut reader: impl AsyncRead + Unpin,
    kept: &mut Vec<u8>,
    total: &mut usize,
) -> std::io::Result<()> {
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        *total = total.saturating_add(read);
        let take = read.min(MAX_OUTPUT_BYTES - kept.len());
        kept.extend_from_slice(&buffer[..take]);
        // Continue draining after the cap, so a verbose child cannot deadlock
        // while trying to fill an unread pipe.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CheckConfig;

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

    #[tokio::test]
    async fn output_is_bounded_but_pipe_is_fully_drained() {
        let source = vec![b'x'; MAX_OUTPUT_BYTES * 3];
        let mut kept = Vec::new();
        let mut total = 0;
        capture(source.as_slice(), &mut kept, &mut total)
            .await
            .unwrap();
        assert_eq!(kept.len(), MAX_OUTPUT_BYTES);
        assert_eq!(total, source.len());
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
                    "checks::tests::slow_check_fixture".into(),
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
