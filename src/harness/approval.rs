//! How the calls that need approval are answered in a run: by the approval
//! mode of its environment or command line.

use super::auto_approval::AutoApproval;
use crate::{
    application::{
        agent::ModelTarget,
        approval::{AlwaysApprove, DenyApproval},
        ports::ApprovalHandler,
    },
    domain::approval::ApprovalMode,
};
use std::sync::Arc;

/// What an approval handler is built from, so it can be built again for
/// another reviewer model when a conversation switches models.
#[derive(Clone)]
pub struct ApprovalFactory {
    pub mode: ApprovalMode,
    /// Answers the requests of the `ask` mode: a prompt on a terminal or in
    /// the browser, a denial when nobody can answer.
    pub ask_user: Arc<dyn ApprovalHandler>,
}

impl ApprovalFactory {
    /// The handler for `mode`. `auto` reviews with `reviewer` and denies
    /// what it does not clearly allow, without asking the user.
    pub fn build(&self, reviewer: ModelTarget) -> Arc<dyn ApprovalHandler> {
        match self.mode {
            ApprovalMode::Allow => Arc::new(AlwaysApprove),
            ApprovalMode::Deny => Arc::new(DenyApproval),
            ApprovalMode::Ask => Arc::clone(&self.ask_user),
            ApprovalMode::Auto => Arc::new(
                AutoApproval::new(reviewer.client, reviewer.model)
                    .with_reasoning_effort(reviewer.reasoning_effort),
            ),
        }
    }
}
