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
        character::CharacterStore, chronotope::Chronotope, objectives::ObjectiveBook,
        project::read_project_instructions, routines::RoutineBook, skills::SkillLibrary,
        tools::register_builtin_tools, user_memory::MemoryBook,
    },
};
use anyhow::Result;
use std::sync::Arc;

/// The tools and stores the agents of one process share.
#[derive(Clone)]
pub struct Harness {
    /// The built-in tools, with those of the raw history, skills, the
    /// character, memory, objectives, and routines when they are enabled.
    pub registry: ToolRegistry,
    pub history: Option<Arc<Chronotope>>,
    pub skills: Option<Arc<SkillLibrary>>,
    pub character: Option<Arc<CharacterStore>>,
    pub memory: Option<Arc<MemoryBook>>,
    pub objectives: Option<Arc<ObjectiveBook>>,
    pub routines: Option<Arc<RoutineBook>>,
}

impl Harness {
    /// Register the built-in tools, and set up the raw history and skills
    /// that `config` enables.
    pub fn new(config: &AppConfig) -> Result<Self> {
        let registry = ToolRegistry::new();
        register_builtin_tools(&registry)?;
        Self::with_registry(registry, config)
    }

    /// A harness with only the tools of `registry`: no raw history, skills,
    /// or character. For tests that build their own.
    #[cfg(test)]
    pub(crate) fn bare(registry: ToolRegistry) -> Self {
        Self {
            registry,
            history: None,
            skills: None,
            character: None,
            memory: None,
            objectives: None,
            routines: None,
        }
    }

    /// Set up the raw history and skills that `config` enables, adding their
    /// tools to `registry`, which holds the other tools. The raw history
    /// needs its token, so callers set it up only once the rest of a command
    /// has been checked.
    pub fn with_registry(registry: ToolRegistry, config: &AppConfig) -> Result<Self> {
        let history = Chronotope::from_settings(&config.history, &registry)?;
        let skills = SkillLibrary::from_settings(&config.skills, &registry)?;
        let character = CharacterStore::from_settings(&config.character, &registry)?;
        let memory = MemoryBook::from_settings(&config.memory, &registry)?;
        let objectives = ObjectiveBook::from_settings(&config.objectives, &registry)?;
        let routines = RoutineBook::from_settings(&config.routines, &registry)?;
        Ok(Self {
            registry,
            history,
            skills,
            character,
            memory,
            objectives,
            routines,
        })
    }

    /// Append the user's character, memory, objectives, and routines, the
    /// project instructions of
    /// the workspace (such as `AGENTS.md`), and the list of the user's skills
    /// to the instructions of `profile`. It reads files, so call it off the
    /// async runtime where that matters. Returns warnings about the files
    /// that were left out: a broken character, an unreadable memory, or
    /// skills.
    pub fn add_instructions(&self, profile: &mut ExecutionProfile) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        if let Some(character) = &self.character {
            if let Some(problem) = profile.append_character(character) {
                warnings.push(format!("ignored the character: {problem}"));
            }
        }
        if let Some(memory) = &self.memory {
            if let Err(error) = profile.append_memory(memory) {
                warnings.push(format!("left out the memory: {error:#}"));
            }
        }
        if let Some(objectives) = &self.objectives {
            if let Err(error) = profile.append_objectives(objectives) {
                warnings.push(format!("left out the objectives: {error:#}"));
            }
        }
        if let Some(routines) = &self.routines {
            if let Err(error) = profile.append_routines(routines) {
                warnings.push(format!("left out the routines: {error:#}"));
            }
        }
        if let Some(workspace) = &profile.context.workspace {
            let sources = read_project_instructions(workspace, &profile.project_instructions)?;
            profile.append_project_instructions(&sources);
        }
        if let Some(skills) = &self.skills {
            warnings.extend(
                profile
                    .append_skills(skills)?
                    .into_iter()
                    .map(|problem| format!("skipped skill {problem}")),
            );
        }
        Ok(warnings)
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
        domain::{approval::ApprovalMode, character::Character, skill::Skill},
        harness::settings::AgentConfig,
    };

    #[test]
    fn runs_get_the_character_the_workspaces_instructions_and_the_users_skills() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("AGENTS.md"), "Run cargo fmt.\n").unwrap();
        let skills = tempfile::tempdir().unwrap();
        let library = SkillLibrary::new(skills.path());
        let skill = Skill::new("release-build", "Build a release.", "1. Build.").unwrap();
        library.save("alice", &skill).unwrap();
        let characters = tempfile::tempdir().unwrap();
        let store = CharacterStore::new(characters.path());
        let character = Character::new("Mira", "A curious partner.", "Speaks warmly.").unwrap();
        store.save("alice", &character).unwrap();
        let harness = Harness {
            registry: ToolRegistry::new(),
            history: None,
            skills: Some(Arc::new(library)),
            character: Some(Arc::new(store)),
            memory: None,
            objectives: None,
            routines: None,
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
            personal_context: true,
        };

        let problems = harness.add_instructions(&mut profile).unwrap();

        assert!(problems.is_empty());
        let instructions = &profile.settings.instructions;
        assert!(instructions.starts_with(&config.settings.instructions));
        let character = instructions
            .find("\n\nName: Mira\nTagline: A curious partner.\n\nSpeaks warmly.")
            .unwrap();
        let project = instructions
            .find("# Project instructions from AGENTS.md\n")
            .unwrap();
        let skills = instructions
            .find("- release-build: Build a release.")
            .unwrap();
        assert!(character < project && project < skills, "{instructions}");
    }

    #[test]
    fn a_broken_character_file_is_left_out_with_a_warning() {
        let characters = tempfile::tempdir().unwrap();
        std::fs::create_dir(characters.path().join("alice")).unwrap();
        std::fs::write(characters.path().join("alice/SOUL.md"), "no frontmatter").unwrap();
        let harness = Harness {
            registry: ToolRegistry::new(),
            history: None,
            skills: None,
            character: Some(Arc::new(CharacterStore::new(characters.path()))),
            memory: None,
            objectives: None,
            routines: None,
        };
        let config = AgentConfig::default();
        let mut profile = ExecutionProfile {
            settings: config.settings.clone(),
            project_instructions: Vec::new(),
            policy: Default::default(),
            context: crate::domain::tool::ToolContext {
                user_id: "alice".into(),
                ..Default::default()
            },
            approval_mode: ApprovalMode::Ask,
            personal_context: true,
        };

        let warnings = harness.add_instructions(&mut profile).unwrap();

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with("ignored the character: "),
            "{warnings:?}"
        );
        assert_eq!(profile.settings.instructions, config.settings.instructions);
    }
}
