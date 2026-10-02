//! Terminal prompt for tool approval.

use super::output::ProgressHold;
use crate::application::ports::{ApprovalHandler, McpApprovalRequest};
use anyhow::{Context, Result};
use async_trait::async_trait;

/// Prompts on stderr and reads the answer from stdin. Only use this when
/// stdin is an interactive terminal.
pub struct InteractiveApproval;

#[async_trait]
impl ApprovalHandler for InteractiveApproval {
    async fn approve(&self, request: McpApprovalRequest) -> Result<bool> {
        let prompt = format!(
            "\n{}: {}\nArguments: {}\nAllow this call? [y/N] ",
            request.heading(),
            request.target(),
            request.arguments
        );
        tokio::task::spawn_blocking(move || {
            use std::io::{self, Write};
            let _hold = ProgressHold::start();
            eprint!("{prompt}");
            io::stderr().flush().ok();
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            Ok::<bool, std::io::Error>(matches!(
                answer.trim().to_ascii_lowercase().as_str(),
                "y" | "yes"
            ))
        })
        .await
        .context("interactive approval task failed")?
        .context("failed to read approval response")
    }
}
