//! Assembly of the harness's agents from the config file: the run loop with
//! the built-in tools, the instructions of the workspace and the user's
//! skills, the models of its roles, its approval mode, the review gate, and
//! the raw history.

use super::{
    approval::ApprovalFactory, models::RunModels, profile::ExecutionProfile, review::ReviewGate,
};
use crate::{
    application::{agent::Agent, ports::McpGateway, registry::ToolRegistry},
    config::AppConfig,
    infrastructure::{
        chronotope::Chronotope, project::read_project_instructions, skills::SkillLibrary,
        tools::register_builtin_tools,
    },
};
use anyhow::Result;
use std::sync::Arc;

/// The tools and stores the agents of one process share.
#[derive(Clone)]
pub struct Harness {
    /// The built-in tools, with those of the raw history and of skills when
    /// they are enabled.
    pub registry: ToolRegistry,
    pub history: Option<Arc<Chronotope>>,
    pub skills: Option<Arc<SkillLibrary>>,
}

impl Harness {
    /// Register the built-in tools, and set up the raw history and skills
    /// that `config` enables.
    pub fn new(config: &AppConfig) -> Result<Self> {
        let registry = ToolRegistry::new();
        register_builtin_tools(&registry)?;
        Self::with_registry(registry, config)
    }

    /// Set up the raw history and skills that `config` enables, adding their
    /// tools to `registry`, which holds the other tools. The raw history
    /// needs its token, so callers set it up only once the rest of a command
    /// has been checked.
    pub fn with_registry(registry: ToolRegistry, config: &AppConfig) -> Result<Self> {
        let history = Chronotope::from_settings(&config.history, &registry)?;
        let skills = SkillLibrary::from_settings(&config.skills, &registry)?;
        Ok(Self {
            registry,
            history,
            skills,
        })
    }

    /// Append the project instructions of the workspace (such as
    /// `AGENTS.md`) and the list of the user's skills to the instructions of
    /// `profile`. It reads files, so call it off the async runtime where
    /// that matters. Returns the problems of the skills that were left out.
    pub fn add_instructions(&self, profile: &mut ExecutionProfile) -> Result<Vec<String>> {
        if let Some(workspace) = &profile.context.workspace {
            let sources = read_project_instructions(workspace, &profile.project_instructions)?;
            profile.append_project_instructions(&sources);
        }
        match &self.skills {
            Some(skills) => profile.append_skills(skills),
            None => Ok(Vec::new()),
        }
    }

    /// An agent for a run of `profile` on `models`, whose calls that need
    /// approval are answered as `approval` says. `mcp` can be shared by the
    /// agents of a process to reuse MCP connections.
    pub fn agent(
        &self,
        profile: &ExecutionProfile,
        models: RunModels,
        mcp: Arc<dyn McpGateway>,
        approval: &ApprovalFactory,
    ) -> Agent {
        let mut agent = Agent::new(
            models.main.client,
            profile.settings.clone(),
            mcp,
            self.registry.clone(),
            profile.policy.clone(),
            approval.build(models.approval),
        )
        .with_context_window(models.main.context_window)
        .with_subagent_models(models.subagents)
        .with_extension(Arc::new(ReviewGate::new()));
        if let Some(history) = &self.history {
            agent = agent.with_history(history.clone());
        }
        agent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{approval::ApprovalMode, skill::Skill},
        harness::settings::AgentConfig,
    };

    #[test]
    fn runs_get_the_workspaces_instructions_and_the_users_skills() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("AGENTS.md"), "Run cargo fmt.\n").unwrap();
        let skills = tempfile::tempdir().unwrap();
        let library = SkillLibrary::new(skills.path());
        let skill = Skill::new("release-build", "Build a release.", "1. Build.").unwrap();
        library.save("alice", &skill).unwrap();
        let harness = Harness {
            registry: ToolRegistry::new(),
            history: None,
            skills: Some(Arc::new(library)),
        };
        let config = AgentConfig::default();
        let mut profile = ExecutionProfile {
            settings: config.settings.clone(),
            project_instructions: config.project_instructions.clone(),
            policy: Default::default(),
            context: crate::domain::tool::ToolContext {
                user_id: "alice".into(),
                workspace: Some(workspace.path().into()),
                ..Default::default()
            },
            approval_mode: ApprovalMode::Ask,
        };

        let problems = harness.add_instructions(&mut profile).unwrap();

        assert!(problems.is_empty());
        let instructions = &profile.settings.instructions;
        assert!(instructions.starts_with(&config.settings.instructions));
        let project = instructions
            .find("# Project instructions from AGENTS.md\n")
            .unwrap();
        let skills = instructions
            .find("- release-build: Build a release.")
            .unwrap();
        assert!(project < skills, "{instructions}");
    }
}
