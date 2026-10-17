//! Resolution of a named environment, or of the permissions a run chose
//! itself, into the settings of one run.

use super::{
    instructions::{append_project_instructions, append_skills},
    settings::AgentConfig,
};
use crate::{
    application::settings::AgentSettings,
    config::{AppConfig, ModelRequest, ModelSelection},
    domain::{
        approval::ApprovalMode,
        environment::EnvironmentConfig,
        policy::UserPolicy,
        skill::{SKILL_READ_NAME, SKILL_SAVE_NAME},
        tool::ToolContext,
    },
    infrastructure::skills::SkillLibrary,
};
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Everything a run takes from configuration: model settings, the effective
/// tool policy, and the context passed to tools.
#[derive(Debug, Clone)]
pub struct ExecutionProfile {
    pub settings: AgentSettings,
    /// Files in the workspace root whose contents are appended to the
    /// instructions (see `AgentConfig::project_instructions`).
    pub project_instructions: Vec<String>,
    pub policy: UserPolicy,
    pub context: ToolContext,
    pub approval_mode: ApprovalMode,
}

/// What a run that names no configured environment may do, as its command
/// line or its client chose.
#[derive(Debug, Clone, Default)]
pub struct Permissions {
    pub workspace: Option<PathBuf>,
    pub allow_writes: bool,
    pub allow_exec: bool,
}

impl ExecutionProfile {
    /// The settings of a run that names no configured environment: those of
    /// `[agent]` with the user's policy and `permissions`, under the
    /// environment name `environment`.
    pub fn ad_hoc(
        base: &AgentConfig,
        user_policy: UserPolicy,
        user_id: &str,
        environment: &str,
        permissions: Permissions,
    ) -> Self {
        Self {
            settings: base.settings.clone(),
            project_instructions: base.project_instructions.clone(),
            policy: user_policy,
            context: ToolContext {
                user_id: user_id.to_string(),
                environment: environment.to_string(),
                workspace: permissions.workspace,
                allow_writes: permissions.allow_writes,
                allow_exec: permissions.allow_exec,
                checks: Default::default(),
            },
            approval_mode: base.approval_mode,
        }
    }

    /// Choose the model of the run from `layers`, the requests from the
    /// bottom up, and apply it to the settings, which must then be valid.
    pub fn choose_model(
        &mut self,
        config: &AppConfig,
        layers: &[ModelRequest],
    ) -> Result<ModelSelection> {
        let selection = config
            .select_model(layers)
            .context("invalid model choice")?;
        selection.apply_to(&mut self.settings);
        self.settings.validate()?;
        Ok(selection)
    }

    /// Layer a named environment over the base settings and the user's
    /// policy. The result never allows a tool that either forbids.
    pub fn for_environment(
        base: &AgentConfig,
        user_policy: &UserPolicy,
        user_id: &str,
        name: &str,
        environment: &EnvironmentConfig,
    ) -> Self {
        let mut settings = base.settings.clone();
        if let Some(model) = &environment.model {
            settings.model.clone_from(model);
        }
        if let Some(instructions) = &environment.instructions {
            settings.instructions.clone_from(instructions);
        }
        Self {
            settings,
            project_instructions: base.project_instructions.clone(),
            policy: user_policy.with_restrictions(
                environment.allowed_tools.as_deref(),
                &environment.disabled_tools,
            ),
            context: ToolContext {
                user_id: user_id.to_string(),
                environment: name.to_string(),
                workspace: environment.workspace.clone(),
                allow_writes: environment.allow_writes,
                allow_exec: environment.allow_exec,
                checks: environment.checks.clone(),
            },
            approval_mode: environment.effective_approval_mode(),
        }
    }

    /// Tell the run about its user's skills in `library`, when it may read
    /// them. Saving is mentioned only when `skill_save` is allowed and can
    /// be approved. Returns the problems of skills that were left out.
    pub fn append_skills(&mut self, library: &SkillLibrary) -> Result<Vec<String>> {
        let available = |name: &str| self.policy.is_allowed(name) && !self.policy.is_disabled(name);
        if !available(SKILL_READ_NAME) {
            return Ok(Vec::new());
        }
        let can_save = available(SKILL_SAVE_NAME) && self.approval_mode != ApprovalMode::Deny;
        let listing = library.list(&self.context.user_id)?;
        append_skills(&mut self.settings.instructions, &listing.skills, can_save);
        Ok(listing.problems)
    }

    /// Append project instructions read from the workspace, as
    /// `read_project_instructions` returns them.
    pub fn append_project_instructions(&mut self, sources: &[(String, String)]) {
        append_project_instructions(&mut self.settings.instructions, sources);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::skill::Skill;

    #[test]
    fn instructions_list_skills_and_mention_saving_only_when_it_can_succeed() {
        let root = tempfile::tempdir().unwrap();
        let library = SkillLibrary::new(root.path());
        library
            .save(
                "default",
                &Skill::new(
                    "release-build",
                    "Use for release-build: it says so.",
                    "1. Build.",
                )
                .unwrap(),
            )
            .unwrap();
        let profile = |policy: UserPolicy, approval_mode| ExecutionProfile {
            settings: Default::default(),
            project_instructions: Vec::new(),
            policy,
            context: ToolContext {
                user_id: "default".into(),
                ..ToolContext::default()
            },
            approval_mode,
        };

        let mut run = profile(UserPolicy::default(), ApprovalMode::Ask);
        let base = run.settings.instructions.clone();
        run.append_skills(&library).unwrap();
        let added = &run.settings.instructions[base.len()..];
        assert!(
            added.contains("- release-build: Use for release-build: it says so."),
            "{added}"
        );
        assert!(added.contains(SKILL_SAVE_NAME), "{added}");

        let mut denied = profile(UserPolicy::default(), ApprovalMode::Deny);
        denied.append_skills(&library).unwrap();
        assert!(denied.settings.instructions.contains(SKILL_READ_NAME));
        assert!(!denied.settings.instructions.contains(SKILL_SAVE_NAME));

        let mut restricted = profile(
            UserPolicy::new(Vec::new(), Some(vec!["workspace_*".into()])),
            ApprovalMode::Ask,
        );
        restricted.append_skills(&library).unwrap();
        assert_eq!(restricted.settings.instructions, base);
    }
}
