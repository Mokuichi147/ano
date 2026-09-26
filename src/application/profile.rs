//! Resolution of a named environment into the settings of one run.

use crate::{
    application::{
        approval::{AlwaysApprove, DenyApproval},
        ports::ApprovalHandler,
        settings::AgentSettings,
    },
    domain::{environment::EnvironmentConfig, policy::UserPolicy, tool::ToolContext},
};
use std::sync::Arc;

/// Everything a run takes from configuration: model settings, the effective
/// tool policy, and the context passed to tools.
#[derive(Debug, Clone)]
pub struct ExecutionProfile {
    pub settings: AgentSettings,
    pub policy: UserPolicy,
    pub context: ToolContext,
    pub auto_approve_mcp: bool,
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
            auto_approve_mcp: environment.auto_approve_mcp,
        }
    }

    /// Approval for runs without an interactive terminal: deny unless the
    /// environment explicitly opts in.
    pub fn unattended_approval(&self) -> Arc<dyn ApprovalHandler> {
        if self.auto_approve_mcp {
            Arc::new(AlwaysApprove)
        } else {
            Arc::new(DenyApproval)
        }
    }
}
