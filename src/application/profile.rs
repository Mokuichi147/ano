//! Resolution of a named environment into the settings of one run.

use crate::{
    application::{
        approval::{AlwaysApprove, DenyApproval},
        auto_approval::AutoApproval,
        ports::{ApprovalHandler, ResponsesApi},
        settings::AgentSettings,
    },
    domain::{
        approval::ApprovalMode, environment::EnvironmentConfig, policy::UserPolicy,
        tool::ToolContext,
    },
};
use std::sync::Arc;

/// Everything a run takes from configuration: model settings, the effective
/// tool policy, and the context passed to tools.
#[derive(Debug, Clone)]
pub struct ExecutionProfile {
    pub settings: AgentSettings,
    pub policy: UserPolicy,
    pub context: ToolContext,
    pub approval_mode: ApprovalMode,
}

impl ExecutionProfile {
    /// Layer a named environment over the base settings and the user's
    /// policy. The result never allows a tool that either forbids.
    pub fn for_environment(
        base: &AgentSettings,
        user_policy: &UserPolicy,
        user_id: &str,
        name: &str,
        environment: &EnvironmentConfig,
    ) -> Self {
        let mut settings = base.clone();
        if let Some(model) = &environment.model {
            settings.model.clone_from(model);
        }
        if let Some(instructions) = &environment.instructions {
            settings.instructions.clone_from(instructions);
        }
        Self {
            settings,
            policy: user_policy.with_restrictions(
                environment.allowed_tools.as_deref(),
                &environment.disabled_tools,
            ),
            context: ToolContext {
                user_id: user_id.to_string(),
                environment: name.to_string(),
                workspace: environment.workspace.clone(),
                allow_writes: environment.allow_writes,
                checks: environment.checks.clone(),
            },
            approval_mode: environment.effective_approval_mode(),
        }
    }

    /// Build the approval handler for this run. `ask` defers to `ask_user`,
    /// which is a denial for runs nobody can answer; `auto` reviews with
    /// `client` and passes uncertain calls to `ask_user`.
    pub fn approval_handler(
        &self,
        client: Arc<dyn ResponsesApi>,
        ask_user: Arc<dyn ApprovalHandler>,
    ) -> Arc<dyn ApprovalHandler> {
        match self.approval_mode {
            ApprovalMode::Allow => Arc::new(AlwaysApprove),
            ApprovalMode::Deny => Arc::new(DenyApproval),
            ApprovalMode::Ask => ask_user,
            ApprovalMode::Auto => Arc::new(AutoApproval::new(
                client,
                self.settings.reviewer_model(),
                ask_user,
            )),
        }
    }
}
