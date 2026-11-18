//! Resolution of a named environment, or of the permissions a run chose
//! itself, into the settings of one run.

use super::{
    instructions::{
        append_character, append_feed, append_memory, append_objectives,
        append_project_instructions, append_routines, append_skills, append_spaces,
    },
    settings::AgentConfig,
};
use crate::{
    application::settings::AgentSettings,
    config::{AppConfig, ModelRequest, ModelSelection},
    domain::{
        approval::ApprovalMode,
        character::CHARACTER_UPDATE_NAME,
        environment::EnvironmentConfig,
        feed::{FEED_POST_NAME, FEED_READ_NAME},
        objective::{OBJECTIVE_LOG_NAME, OBJECTIVE_READ_NAME, OBJECTIVE_SET_NAME},
        policy::UserPolicy,
        routine::{ROUTINE_CREATE_NAME, ROUTINE_DELETE_NAME},
        skill::{SKILL_READ_NAME, SKILL_SAVE_NAME},
        space::{SPACE_DATA_NAME, SPACE_READ_NAME, SPACE_SAVE_NAME},
        tool::{ToolContext, TOOL_SEARCH_NAME},
        user_memory::{MEMORY_FORGET_NAME, MEMORY_SAVE_NAME},
    },
    infrastructure::{
        automation::schedule::local_time, character::CharacterStore, objectives::ObjectiveBook,
        routines::RoutineBook, skills::SkillLibrary, spaces::Spaces, user_memory::MemoryBook,
    },
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
            OBJECTIVE_SET_NAME,
            OBJECTIVE_LOG_NAME,
            OBJECTIVE_READ_NAME,
            ROUTINE_CREATE_NAME,
            ROUTINE_DELETE_NAME,
            FEED_POST_NAME,
            FEED_READ_NAME,
            SPACE_SAVE_NAME,
            SPACE_READ_NAME,
            SPACE_DATA_NAME,
        ]);
    }

    /// Whether the run can load tools with `tool_search`. The tools that only
    /// read what a feature keeps are not offered up front, to keep requests
    /// small, so the instructions mention them only when they can be loaded.
    fn can_search(&self) -> bool {
        !self.policy.is_disabled(TOOL_SEARCH_NAME)
    }

    /// Tell the run about the user's spaces in `spaces`, when it may build
    /// them and keeps personal context.
    pub fn append_spaces(&mut self, spaces: &Spaces) -> Result<()> {
        let available = |name: &str| self.policy.is_allowed(name) && !self.policy.is_disabled(name);
        if !self.personal_context
            || ![SPACE_SAVE_NAME, SPACE_READ_NAME, SPACE_DATA_NAME]
                .iter()
                .all(|name| available(name))
        {
            return Ok(());
        }
        let listed: Vec<String> = spaces
            .list(&self.context.user_id)?
            .into_iter()
            .map(|space| format!("{}: {}", space.name, space.title))
            .collect();
        append_spaces(&mut self.settings.instructions, &listed);
        Ok(())
    }

    /// Tell the run about the user's feed, when it may post to it and keeps
    /// personal context. Reading recent items first is mentioned only when
    /// `feed_read` can be loaded.
    pub fn append_feed(&mut self) {
        let available = |name: &str| self.policy.is_allowed(name) && !self.policy.is_disabled(name);
        if self.personal_context && available(FEED_POST_NAME) {
            let can_read = available(FEED_READ_NAME) && self.can_search();
            append_feed(&mut self.settings.instructions, can_read);
        }
    }

    /// Tell the run the current time and its user's upcoming routines in
    /// `book`. Setting up and removing are mentioned only when their tools
    /// are allowed and can be approved. Nothing is added when the run keeps
    /// out personal context.
    pub fn append_routines(&mut self, book: &RoutineBook) -> Result<()> {
        if !self.personal_context {
            return Ok(());
        }
        let available = |name: &str| {
            self.policy.is_allowed(name)
                && !self.policy.is_disabled(name)
                && self.approval_mode != ApprovalMode::Deny
        };
        let now = crate::application::clock::unix_now();
        let upcoming: Vec<String> = book
            .upcoming(&self.context.user_id, now)?
            .into_iter()
            .map(|(at, routine)| {
                let repeat = match &routine.cron {
                    Some(pattern) => format!("repeats \"{pattern}\""),
                    None => "once".into(),
                };
                format!(
                    "[{}] {} {} at {} ({repeat}): {}",
                    routine.id,
                    routine.kind.name(),
                    routine.title,
                    local_time(at),
                    crate::domain::text::cut(&routine.task.replace('\n', " "), 120)
                )
            })
            .collect();
        append_routines(
            &mut self.settings.instructions,
            &chrono::Local::now()
                .format("%Y-%m-%d %H:%M %:z (%A)")
                .to_string(),
            &upcoming,
            available(ROUTINE_CREATE_NAME),
            available(ROUTINE_DELETE_NAME),
        );
        Ok(())
    }

    /// Tell the run about its user's objectives in `book`. Creating and
    /// changing are mentioned only when `objective_set` is allowed and can
    /// be approved, recording progress when `objective_log` is allowed, and
    /// reading the whole list when `objective_read` can be loaded. Nothing
    /// is added when the run keeps out personal context or cannot
    /// read objectives.
    pub fn append_objectives(&mut self, book: &ObjectiveBook) -> Result<()> {
        let available = |name: &str| self.policy.is_allowed(name) && !self.policy.is_disabled(name);
        if !self.personal_context || !available(OBJECTIVE_READ_NAME) {
            return Ok(());
        }
        let can_set = available(OBJECTIVE_SET_NAME) && self.approval_mode != ApprovalMode::Deny;
        let can_log = available(OBJECTIVE_LOG_NAME);
        let can_read = self.can_search();
        let objectives = book.list(&self.context.user_id)?;
        append_objectives(
            &mut self.settings.instructions,
            &objectives,
            can_set,
            can_log,
            can_read,
        );
        Ok(())
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
            OBJECTIVE_SET_NAME,
            OBJECTIVE_LOG_NAME,
            OBJECTIVE_READ_NAME,
            ROUTINE_CREATE_NAME,
            ROUTINE_DELETE_NAME,
            FEED_POST_NAME,
            FEED_READ_NAME,
            SPACE_SAVE_NAME,
            SPACE_READ_NAME,
            SPACE_DATA_NAME,
        ] {
            assert!(untrusted.policy.is_disabled(name), "{name}");
        }
        let mut untrusted_feed = untrusted.clone();
        let before = untrusted_feed.settings.instructions.clone();
        untrusted_feed.append_feed();
        assert_eq!(untrusted_feed.settings.instructions, before);
    }

    #[test]
    fn instructions_list_active_objectives_and_how_to_help_with_them() {
        let root = tempfile::tempdir().unwrap();
        let book = ObjectiveBook::new(root.path());
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
        empty.append_objectives(&book).unwrap();
        let added = &empty.settings.instructions[base.len()..];
        assert!(added.starts_with("\n\n# Objectives\n"), "{added}");
        assert!(added.ends_with("No objective is active."), "{added}");
        assert!(added.contains(OBJECTIVE_SET_NAME) && added.contains(OBJECTIVE_LOG_NAME));
        assert!(
            added.contains(&format!(
                "load {OBJECTIVE_READ_NAME} with {TOOL_SEARCH_NAME}"
            )),
            "{added}"
        );
        // Without tool_search, objective_read cannot be loaded.
        let mut unsearchable = profile(
            UserPolicy::default().with_extra_disabled([TOOL_SEARCH_NAME]),
            ApprovalMode::Ask,
        );
        unsearchable.append_objectives(&book).unwrap();
        let added = &unsearchable.settings.instructions[base.len()..];
        assert!(added.starts_with("\n\n# Objectives\n"), "{added}");
        assert!(!added.contains(OBJECTIVE_READ_NAME), "{added}");

        book.create(
            "default",
            &crate::domain::objective::ObjectiveChange {
                title: Some("Read 12 books".into()),
                next_step: Some("Pick the first book".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let mut denied = profile(UserPolicy::default(), ApprovalMode::Deny);
        denied.append_objectives(&book).unwrap();
        let added = &denied.settings.instructions[base.len()..];
        assert!(
            added.ends_with("\n- [o1] Read 12 books; next: Pick the first book"),
            "{added}"
        );
        assert!(!added.contains(OBJECTIVE_SET_NAME), "{added}");
        assert!(added.contains(OBJECTIVE_LOG_NAME), "{added}");

        let mut untrusted = profile(UserPolicy::default(), ApprovalMode::Ask);
        untrusted.without_personal_context();
        untrusted.append_objectives(&book).unwrap();
        assert_eq!(untrusted.settings.instructions, base);
    }

    #[test]
    fn instructions_give_the_time_and_upcoming_routines() {
        let root = tempfile::tempdir().unwrap();
        let book = RoutineBook::new(root.path());
        let profile = |approval_mode| ExecutionProfile {
            settings: Default::default(),
            project_instructions: Vec::new(),
            policy: UserPolicy::default(),
            context: ToolContext {
                user_id: "default".into(),
                ..ToolContext::default()
            },
            approval_mode,
            personal_context: true,
        };
        let mut empty = profile(ApprovalMode::Ask);
        let base = empty.settings.instructions.clone();
        empty.append_routines(&book).unwrap();
        let added = &empty.settings.instructions[base.len()..];
        assert!(
            added.starts_with("\n\n# Routines\nThis conversation started at 20"),
            "{added}"
        );
        assert!(added.contains(ROUTINE_CREATE_NAME) && added.ends_with("No routine is set up."));

        book.create(
            "default",
            crate::domain::routine::RoutineKind::Task,
            "Weekly review",
            "Sum up my week.",
            Some("0 17 * * FRI"),
            None,
        )
        .unwrap();
        let mut denied = profile(ApprovalMode::Deny);
        denied.append_routines(&book).unwrap();
        let added = &denied.settings.instructions[base.len()..];
        assert!(added.contains("\n- [r1] task Weekly review at "), "{added}");
        assert!(
            added.ends_with("(repeats \"0 17 * * FRI\"): Sum up my week."),
            "{added}"
        );
        assert!(!added.contains(ROUTINE_CREATE_NAME), "{added}");
    }

    #[test]
    fn feed_instructions_mention_reading_only_when_it_can_be_loaded() {
        let profile = |policy: UserPolicy| ExecutionProfile {
            settings: Default::default(),
            project_instructions: Vec::new(),
            policy,
            context: ToolContext::default(),
            approval_mode: ApprovalMode::Ask,
            personal_context: true,
        };
        let mut searchable = profile(UserPolicy::default());
        let base = searchable.settings.instructions.clone();
        searchable.append_feed();
        let added = &searchable.settings.instructions[base.len()..];
        assert!(added.starts_with("\n\n# Feed\n"), "{added}");
        assert!(
            added.contains(&format!("load {FEED_READ_NAME} with {TOOL_SEARCH_NAME}")),
            "{added}"
        );

        let mut unsearchable =
            profile(UserPolicy::default().with_extra_disabled([TOOL_SEARCH_NAME]));
        unsearchable.append_feed();
        let added = &unsearchable.settings.instructions[base.len()..];
        assert!(added.contains(FEED_POST_NAME), "{added}");
        assert!(!added.contains(FEED_READ_NAME), "{added}");

        let mut untrusted = profile(UserPolicy::default());
        untrusted.without_personal_context();
        untrusted.append_feed();
        assert_eq!(untrusted.settings.instructions, base);
    }
}
