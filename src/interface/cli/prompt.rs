//! Helpers of the interactive `edit` commands.

use anyhow::{bail, Result};
use inquire::InquireError;
use std::io::IsTerminal;

/// `Ok(None)` when the user cancels a prompt.
pub(super) fn answered<T>(answer: Result<T, InquireError>) -> Result<Option<T>> {
    match answer {
        Ok(value) => Ok(Some(value)),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Fail unless stdin and stdout are a terminal, naming the `alternatives`
/// that do the same without prompts.
pub(super) fn require_terminal(command: &str, alternatives: &[&str]) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        let alternatives = alternatives
            .iter()
            .map(|alternative| format!("`{alternative}`"))
            .collect::<Vec<_>>()
            .join(" or ");
        bail!("`{command}` needs a terminal; use {alternatives} instead");
    }
    Ok(())
}
