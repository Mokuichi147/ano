//! Resolution of a named environment, or of the permissions a run chose
//! itself, into the settings of one run.

use super::{
    instructions::{append_character, append_memory, append_project_instructions, append_skills},
    settings::AgentConfig,
};
use crate::{
    application::settings::AgentSettings,
    config::{AppConfig, ModelRequest, ModelSelection},
    domain::{
        approval::ApprovalMode,
        character::CHARACTER_UPDATE_NAME,
        environment::EnvironmentConfig,
        policy::UserPolicy,
        skill::{SKILL_READ_NAME, SKILL_SAVE_NAME},
        tool::ToolContext,
        user_memory::{MEMORY_FORGET_NAME, MEMORY_SAVE_NAME},
    },
    infrastructure::{character::CharacterStore, skills::SkillLibrary, user_memory::MemoryBook},
};
use anyhow::{Context, Result};
use std::path::PathBuf;

/// The tools that read the user's past conversations kept in chronotope
/// (`history_search`, `history_get`, and the like), as a policy rule.
const PAST_CONVERSATIONS: &str = "history_*";

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
    /// Whether the run may see what is known about the user, such as their
    /// memory. Off for runs started by content that third parties wrote.
    pub personal_context: bool,
}

/// What a run that names no configured environment may do, as its command
/// line or its client chose.
#[derive(Debug, Clone, Default)]
pub struct Permissions {
    pub workspace: Option<PathBuf>,
    pub allow_writes: bool,
    pub allow_exec: bool,
    /// Defaults to `[agent].approval_mode`.
    pub approval_mode: Option<ApprovalMode>,
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
            approval_mode: permissions.approval_mode.unwrap_or(base.approval_mode),
            personal_context: true,
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
            personal_context: true,
        }
    }

    /// Tell the run about its user's character in `store`. Changing it is
    /// mentioned only when `character_update` is allowed and can be
    /// approved. A character file that cannot be read is left out, and its
    /// problem returned.
    pub fn append_character(&mut self, store: &CharacterStore) -> Option<String> {
        let can_update = self.policy.is_allowed(CHARACTER_UPDATE_NAME)
            && !self.policy.is_disabled(CHARACTER_UPDATE_NAME)
            && self.approval_mode != ApprovalMode::Deny;
        let (character, problem) = match store.read(&self.context.user_id) {
            Ok(character) => (character, None),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        // A broken file is not offered for replacement: the user may be
        // editing it, and the model cannot see what it held.
        let can_update = can_update && problem.is_none();
        append_character(
            &mut self.settings.instructions,
            character.as_ref(),
            can_update,
        );
        problem
    }

    /// Set this run up for content that third parties wrote, which could
    /// ask the agent to send what it knows about the user somewhere, or to
    /// leave instructions for later runs: keep the user's memory out, and
    /// disable the tools that read the user's past conversations or change
    /// what every later run reads.
    pub fn without_personal_context(&mut self) {
        self.personal_context = false;
        self.policy = self.policy.with_extra_disabled([
            PAST_CONVERSATIONS,
            MEMORY_SAVE_NAME,
            MEMORY_FORGET_NAME,
            CHARACTER_UPDATE_NAME,
            SKILL_SAVE_NAME,
        ]);
    }

    /// Tell the run what `book` remembers about its user. Saving is
    /// mentioned only when `memory_save` is allowed and can be approved,
    /// forgetting when `memory_forget` is allowed. Nothing is added when the
    /// run keeps out personal context.
    pub fn append_memory(&mut self, book: &MemoryBook) -> Result<()> {
        if !self.personal_context {
            return Ok(());
        }
        let available = |name: &str| self.policy.is_allowed(name) && !self.policy.is_disabled(name);
        let can_save = available(MEMORY_SAVE_NAME) && self.approval_mode != ApprovalMode::Deny;
        let can_forget = available(MEMORY_FORGET_NAME) && self.approval_mode != ApprovalMode::Deny;
        let facts = book.facts(&self.context.user_id)?;
        append_memory(
            &mut self.settings.instructions,
            &facts,
            can_save,
            can_forget,
        );
        Ok(())
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
    use crate::domain::{character::Character, skill::Skill};

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
            personal_context: true,
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

    #[test]
    fn instructions_carry_the_character_and_mention_changing_it_only_when_it_can_succeed() {
        let root = tempfile::tempdir().unwrap();
        let store = CharacterStore::new(root.path());
        let profile = |user: &str, policy: UserPolicy, approval_mode| ExecutionProfile {
            settings: Default::default(),
            project_instructions: Vec::new(),
            policy,
            context: ToolContext {
                user_id: user.into(),
                ..ToolContext::default()
            },
            approval_mode,
            personal_context: true,
        };

        // Without a character, the run only learns how to create one.
        let mut none = profile("default", UserPolicy::default(), ApprovalMode::Ask);
        let base = none.settings.instructions.clone();
        assert_eq!(none.append_character(&store), None);
        let added = &none.settings.instructions[base.len()..];
        assert!(added.contains("has not given you a character"), "{added}");
        assert!(added.contains(CHARACTER_UPDATE_NAME), "{added}");
        let mut none_denied = profile("default", UserPolicy::default(), ApprovalMode::Deny);
        none_denied.append_character(&store);
        assert_eq!(none_denied.settings.instructions, base);

        store
            .save(
                "default",
                &Character::new("ミラ", "", "敬語を使わず、短く話す。").unwrap(),
            )
            .unwrap();
        let mut run = profile("default", UserPolicy::default(), ApprovalMode::Ask);
        run.append_character(&store);
        let added = &run.settings.instructions[base.len()..];
        assert!(added.starts_with("\n\n# Character\n"), "{added}");
        assert!(
            added.ends_with("\n\nName: ミラ\n\n敬語を使わず、短く話す。"),
            "{added}"
        );
        assert!(added.contains(CHARACTER_UPDATE_NAME), "{added}");

        // The character stays, but changing it is not offered.
        let mut restricted = profile(
            "default",
            UserPolicy::new(vec![CHARACTER_UPDATE_NAME.into()], None),
            ApprovalMode::Ask,
        );
        restricted.append_character(&store);
        let added = &restricted.settings.instructions[base.len()..];
        assert!(added.contains("Name: ミラ"), "{added}");
        assert!(!added.contains(CHARACTER_UPDATE_NAME), "{added}");

        std::fs::create_dir(root.path().join("broken")).unwrap();
        std::fs::write(store.path("broken").unwrap(), "---\nname: \"\"\n---\n").unwrap();
        let mut broken = profile("broken", UserPolicy::default(), ApprovalMode::Ask);
        assert!(broken.append_character(&store).is_some());
        assert_eq!(broken.settings.instructions, base);
    }

    #[test]
    fn instructions_list_remembered_facts_unless_personal_context_is_kept_out() {
        let root = tempfile::tempdir().unwrap();
        let book = MemoryBook::new(root.path());
        let profile = |policy: UserPolicy, approval_mode| ExecutionProfile {
            settings: Default::default(),
            project_instructions: Vec::new(),
            policy,
            context: ToolContext {
                user_id: "default".into(),
                ..ToolContext::default()
            },
            approval_mode,
            personal_context: true,
        };

        let mut empty = profile(UserPolicy::default(), ApprovalMode::Ask);
        let base = empty.settings.instructions.clone();
        empty.append_memory(&book).unwrap();
        let added = &empty.settings.instructions[base.len()..];
        assert!(
            added.contains("# Memory\n") && added.ends_with("Nothing is remembered yet."),
            "{added}"
        );
        assert!(
            added.contains(MEMORY_SAVE_NAME) && added.contains(MEMORY_FORGET_NAME),
            "{added}"
        );

        book.save("default", "Prefers green tea.").unwrap();
        let mut run = profile(UserPolicy::default(), ApprovalMode::Deny);
        run.append_memory(&book).unwrap();
        let added = &run.settings.instructions[base.len()..];
        assert!(added.ends_with("\n- Prefers green tea."), "{added}");
        assert!(!added.contains(MEMORY_SAVE_NAME), "{added}");
        assert!(!added.contains(MEMORY_FORGET_NAME), "{added}");

        let mut untrusted = profile(UserPolicy::default(), ApprovalMode::Ask);
        untrusted.without_personal_context();
        untrusted.append_memory(&book).unwrap();
        assert_eq!(untrusted.settings.instructions, base);
        for name in [
            "history_search",
            "history_get",
            "history_context",
            "history_conversations",
            MEMORY_SAVE_NAME,
            MEMORY_FORGET_NAME,
            CHARACTER_UPDATE_NAME,
            SKILL_SAVE_NAME,
        ] {
            assert!(untrusted.policy.is_disabled(name), "{name}");
        }
    }
}
