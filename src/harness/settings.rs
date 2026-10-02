//! The `[agent]` table of the config file: the settings of the run loop and
//! those of the harness around it.

use super::instructions::DEFAULT_INSTRUCTIONS;
use crate::{
    application::settings::AgentSettings,
    domain::{approval::ApprovalMode, compaction::CompactionMethod},
};
use anyhow::{bail, Result};
use serde::Deserialize;
use std::path::{Component, Path};

/// The `[agent]` table. Its keys besides the harness's own are those of the
/// run loop's `AgentSettings`, in the same table.
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "AgentTable")]
pub struct AgentConfig {
    /// The settings of the run loop. `instructions` defaults to the
    /// harness's `DEFAULT_INSTRUCTIONS`.
    pub settings: AgentSettings,
    /// Files in the workspace root (for example `AGENTS.md`) whose contents
    /// are appended to the instructions of every run. Missing files are skipped.
    pub project_instructions: Vec<String>,
    /// How MCP approval requests are answered for CLI runs and web UI
    /// sessions without a named environment. Defaults to `auto`.
    pub approval_mode: ApprovalMode,
    /// Reviewer model for `approval_mode = "auto"`; defaults to `model`.
    pub approval_model: Option<String>,
    /// The provider runs use unless one is chosen, from `[providers]`.
    /// Without it, runs connect through `[api]`.
    pub provider: Option<String>,
    /// Presets of the main agent, its sub-agents, and the approval reviewer.
    pub roles: ModelRoles,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            settings: AgentSettings {
                instructions: DEFAULT_INSTRUCTIONS.to_string(),
                ..AgentSettings::default()
            },
            project_instructions: vec!["AGENTS.md".to_string()],
            approval_mode: ApprovalMode::Auto,
            approval_model: None,
            provider: None,
            roles: ModelRoles::default(),
        }
    }
}

/// The `[agent]` table as written, with the keys of the run loop and of the
/// harness side by side, so that an unknown key is reported on its line
/// with every key it could have been.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AgentTable {
    model: String,
    instructions: String,
    max_tool_rounds: usize,
    tool_discovery_limit: usize,
    max_output_tokens: Option<u32>,
    parallel_tool_calls: bool,
    max_parallel_tool_calls: usize,
    tool_timeout_secs: u64,
    max_tool_output_bytes: usize,
    compact_threshold_bytes: Option<usize>,
    compaction: CompactionMethod,
    max_total_tokens: Option<u64>,
    reasoning_effort: Option<String>,
    reasoning_summary: Option<String>,
    project_instructions: Vec<String>,
    approval_mode: ApprovalMode,
    approval_model: Option<String>,
    provider: Option<String>,
    roles: ModelRoles,
}

impl Default for AgentTable {
    fn default() -> Self {
        let AgentConfig {
            settings:
                AgentSettings {
                    model,
                    instructions,
                    max_tool_rounds,
                    tool_discovery_limit,
                    max_output_tokens,
                    parallel_tool_calls,
                    max_parallel_tool_calls,
                    tool_timeout_secs,
                    max_tool_output_bytes,
                    compact_threshold_bytes,
                    compaction,
                    max_total_tokens,
                    reasoning_effort,
                    reasoning_summary,
                },
            project_instructions,
            approval_mode,
            approval_model,
            provider,
            roles,
        } = AgentConfig::default();
        Self {
            model,
            instructions,
            max_tool_rounds,
            tool_discovery_limit,
            max_output_tokens,
            parallel_tool_calls,
            max_parallel_tool_calls,
            tool_timeout_secs,
            max_tool_output_bytes,
            compact_threshold_bytes,
            compaction,
            max_total_tokens,
            reasoning_effort,
            reasoning_summary,
            project_instructions,
            approval_mode,
            approval_model,
            provider,
            roles,
        }
    }
}

impl From<AgentTable> for AgentConfig {
    fn from(table: AgentTable) -> Self {
        let AgentTable {
            model,
            instructions,
            max_tool_rounds,
            tool_discovery_limit,
            max_output_tokens,
            parallel_tool_calls,
            max_parallel_tool_calls,
            tool_timeout_secs,
            max_tool_output_bytes,
            compact_threshold_bytes,
            compaction,
            max_total_tokens,
            reasoning_effort,
            reasoning_summary,
            project_instructions,
            approval_mode,
            approval_model,
            provider,
            roles,
        } = table;
        Self {
            settings: AgentSettings {
                model,
                instructions,
                max_tool_rounds,
                tool_discovery_limit,
                max_output_tokens,
                parallel_tool_calls,
                max_parallel_tool_calls,
                tool_timeout_secs,
                max_tool_output_bytes,
                compact_threshold_bytes,
                compaction,
                max_total_tokens,
                reasoning_effort,
                reasoning_summary,
            },
            project_instructions,
            approval_mode,
            approval_model,
            provider,
            roles,
        }
    }
}

impl AgentConfig {
    pub fn validate(&self) -> Result<()> {
        self.settings.validate()?;
        if self
            .approval_model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty())
        {
            bail!("agent.approval_model must not be empty");
        }
        for name in &self.project_instructions {
            let path = Path::new(name);
            if name.is_empty()
                || !path
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
            {
                bail!("agent.project_instructions entries must be relative paths inside the workspace: {name:?}");
            }
        }
        Ok(())
    }
}

/// The presets (`[presets]` names) of the roles. `default` is the main
/// agent's preset for runs that choose none, applied over `provider`,
/// `model`, and `reasoning_effort` of `[agent]`. A role besides it without a
/// preset uses the main agent's current provider, model, and effort; with
/// one, the preset applies over them, so one that sets only
/// `reasoning_effort` keeps the model and changes the effort.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelRoles {
    /// The main agent, for runs that choose no preset.
    pub default: Option<String>,
    /// Sub-agents started by `delegate_task`.
    pub delegate: Option<String>,
    /// The reviewer started by `review_changes`.
    pub review: Option<String>,
    /// The reviewer of `approval_mode = "auto"`; takes precedence over
    /// `approval_model`.
    pub approval: Option<String>,
}

impl ModelRoles {
    /// Every role with its name in the config and its preset.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, Option<&str>)> {
        [
            ("default", self.default.as_deref()),
            ("delegate", self.delegate.as_deref()),
            ("review", self.review.as_deref()),
            ("approval", self.approval.as_deref()),
        ]
        .into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<AgentConfig, toml::de::Error> {
        toml::from_str(text)
    }

    #[test]
    fn the_table_splits_into_the_run_loops_and_the_harnesss_keys() {
        let config = parse(
            "model = 'local'\nmax_tool_rounds = 7\napproval_mode = 'auto'\napproval_model = 'judge'\nprovider = 'lan'\nproject_instructions = ['CLAUDE.md']\n[roles]\nreview = 'quick'\n",
        )
        .unwrap();
        assert_eq!(config.settings.model, "local");
        assert_eq!(config.settings.max_tool_rounds, 7);
        assert_eq!(config.approval_mode, ApprovalMode::Auto);
        assert_eq!(config.approval_model.as_deref(), Some("judge"));
        assert_eq!(config.provider.as_deref(), Some("lan"));
        assert_eq!(config.project_instructions, ["CLAUDE.md"]);
        assert_eq!(config.roles.review.as_deref(), Some("quick"));
        // Without instructions, runs get the harness's.
        assert_eq!(config.settings.instructions, DEFAULT_INSTRUCTIONS);

        let config = parse("instructions = 'Be brief.'").unwrap();
        assert_eq!(config.settings.instructions, "Be brief.");
        assert_eq!(config.project_instructions, ["AGENTS.md"]);
        assert_eq!(config.approval_mode, ApprovalMode::Auto);
    }

    #[test]
    fn every_key_reaches_its_setting() {
        let config = parse(
            "model = 'm'\ninstructions = 'i'\nmax_tool_rounds = 2\ntool_discovery_limit = 3\nmax_output_tokens = 4\nparallel_tool_calls = false\nmax_parallel_tool_calls = 5\ntool_timeout_secs = 6\nmax_tool_output_bytes = 7000\ncompact_threshold_bytes = 8000\ncompaction = 'summary'\nmax_total_tokens = 9\nreasoning_effort = 'low'\nreasoning_summary = 'concise'\nproject_instructions = []\napproval_mode = 'deny'\napproval_model = 'a'\nprovider = 'p'\n[roles]\ndefault = 'r1'\ndelegate = 'r2'\nreview = 'r3'\napproval = 'r4'\n",
        )
        .unwrap();
        let AgentSettings {
            model,
            instructions,
            max_tool_rounds,
            tool_discovery_limit,
            max_output_tokens,
            parallel_tool_calls,
            max_parallel_tool_calls,
            tool_timeout_secs,
            max_tool_output_bytes,
            compact_threshold_bytes,
            compaction,
            max_total_tokens,
            reasoning_effort,
            reasoning_summary,
        } = config.settings;
        assert_eq!(
            (
                model.as_str(),
                instructions.as_str(),
                max_tool_rounds,
                tool_discovery_limit
            ),
            ("m", "i", 2, 3)
        );
        assert_eq!(max_output_tokens, Some(4));
        assert!(!parallel_tool_calls);
        assert_eq!(
            (
                max_parallel_tool_calls,
                tool_timeout_secs,
                max_tool_output_bytes
            ),
            (5, 6, 7000)
        );
        assert_eq!(compact_threshold_bytes, Some(8000));
        assert_eq!(compaction, CompactionMethod::Summary);
        assert_eq!(max_total_tokens, Some(9));
        assert_eq!(reasoning_effort.as_deref(), Some("low"));
        assert_eq!(reasoning_summary.as_deref(), Some("concise"));
        assert!(config.project_instructions.is_empty());
        assert_eq!(config.approval_mode, ApprovalMode::Deny);
        assert_eq!(config.approval_model.as_deref(), Some("a"));
        assert_eq!(config.provider.as_deref(), Some("p"));
        assert_eq!(
            config.roles,
            ModelRoles {
                default: Some("r1".into()),
                delegate: Some("r2".into()),
                review: Some("r3".into()),
                approval: Some("r4".into()),
            }
        );
    }

    #[test]
    fn unknown_or_mistyped_keys_are_rejected() {
        let unknown = parse("max_tool_round = 7").unwrap_err().to_string();
        assert!(unknown.contains("max_tool_round"), "{unknown}");
        assert!(parse("approval_mode = 'sometimes'").is_err());
        assert!(parse("[roles]\nreviewer = 'quick'").is_err());
        assert!(parse("max_tool_rounds = 'many'").is_err());
    }

    #[test]
    fn the_default_is_the_harnesss() {
        let config = AgentConfig::default();
        assert_eq!(config.settings.instructions, DEFAULT_INSTRUCTIONS);
        assert_ne!(
            AgentSettings::default().instructions,
            DEFAULT_INSTRUCTIONS,
            "the run loop's own default knows no harness tools"
        );
        assert!(!AgentSettings::default().instructions.contains("workspace_"));
        assert_eq!(config.project_instructions, ["AGENTS.md"]);
    }
}
