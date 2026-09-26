//! The configuration file, which gathers the settings of every layer.
//!
//! Each layer owns its settings type; this module only combines them, loads
//! the TOML file, and resolves paths relative to it.

use crate::{
    application::{profile::ExecutionProfile, settings::AgentSettings},
    domain::{environment::EnvironmentConfig, mcp::McpServerConfig, policy::UserPolicy},
    infrastructure::openai::ApiSettings,
    interface::webhook::WebhookSettings,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub api: ApiSettings,
    pub agent: AgentSettings,
    pub mcp_servers: Vec<McpServerConfig>,
    pub users: HashMap<String, UserPolicy>,
    pub environments: HashMap<String, EnvironmentConfig>,
    pub webhook: WebhookSettings,
}

impl AppConfig {
    /// Load and validate a config file. The file must exist, so a mistyped
    /// path never silently falls back to a policy-free default config.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let mut config = Self::parse(&text)
            .with_context(|| format!("invalid config file {}", path.display()))?;
        let absolute_path = std::fs::canonicalize(path)
            .with_context(|| format!("failed to resolve config file {}", path.display()))?;
        let directory = absolute_path
            .parent()
            .context("config file has no parent directory")?;
        for environment in config.environments.values_mut() {
            if let Some(workspace) = &mut environment.workspace {
                if workspace.is_relative() {
                    *workspace = directory.join(&*workspace);
                }
            }
        }
        for server in &mut config.mcp_servers {
            if let Some(cwd) = &mut server.cwd {
                if cwd.is_relative() {
                    *cwd = directory.join(&*cwd);
                }
            }
        }
        Ok(config)
    }

    /// Load `path` when it exists, otherwise use the built-in defaults.
    pub fn load_or_default(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text).context("failed to parse TOML")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.agent.validate()?;
        self.webhook.validate()?;
        if self.api.timeout_secs == 0 {
            bail!("api.timeout_secs must be greater than zero");
        }
        for (name, environment) in &self.environments {
            for (check_name, check) in &environment.checks {
                if check_name.trim().is_empty() {
                    bail!("environments.{name} has an empty check name");
                }
                check
                    .validate()
                    .with_context(|| format!("invalid environments.{name}.checks.{check_name}"))?;
            }
            if environment
                .model
                .as_ref()
                .is_some_and(|model| model.trim().is_empty())
            {
                bail!("environments.{name}.model must not be empty");
            }
        }
        let mut labels = HashSet::new();
        for server in &self.mcp_servers {
            server.validate()?;
            if !labels.insert(server.label.as_str()) {
                bail!("MCP server label '{}' is used more than once", server.label);
            }
        }
        Ok(())
    }

    /// Whether `user_id` has an entry in `[users]`. `default` always exists.
    pub fn has_user(&self, user_id: &str) -> bool {
        user_id == "default" || self.users.contains_key(user_id)
    }

    pub fn policy_for(&self, user_id: &str, extra_disabled: &[String]) -> UserPolicy {
        self.users
            .get(user_id)
            .or_else(|| self.users.get("default"))
            .cloned()
            .unwrap_or_default()
            .with_extra_disabled(extra_disabled.iter().cloned())
    }

    pub fn environment_for(&self, name: &str) -> Result<&EnvironmentConfig> {
        self.environments
            .get(name)
            .with_context(|| format!("unknown execution environment '{name}'"))
    }

    /// Resolve a run of `user_id` in the named environment.
    pub fn execution_profile(
        &self,
        user_id: &str,
        environment: &str,
        extra_disabled: &[String],
    ) -> Result<ExecutionProfile> {
        Ok(ExecutionProfile::for_environment(
            &self.agent,
            &self.policy_for(user_id, extra_disabled),
            user_id,
            environment,
            self.environment_for(environment)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::AppConfig;

    #[test]
    fn default_config_is_valid() {
        let config = AppConfig::default();
        config.validate().unwrap();
        assert_eq!(config.agent.max_tool_rounds, 24);
    }

    #[test]
    fn example_config_parses_and_validates() {
        let config = AppConfig::parse(include_str!("../config.example.toml")).unwrap();
        assert!(config.environments.contains_key("default"));
    }

    #[test]
    fn rejects_misspelled_policy_fields() {
        let error = AppConfig::parse("[users.alice]\ndisable_tools = [\"echo\"]\n").unwrap_err();
        assert!(format!("{error:#}").contains("disable_tools"));
    }

    #[test]
    fn rejects_invalid_approval_mode() {
        let text = "[[mcp_servers]]\nlabel = \"a\"\nurl = \"https://x.test\"\nrequire_approval = \"sometimes\"\n";
        assert!(AppConfig::parse(text).is_err());
    }

    #[test]
    fn rejects_duplicate_or_ambiguous_mcp_labels() {
        let duplicate = "[[mcp_servers]]\nlabel = \"a\"\nurl = \"https://x.test\"\n[[mcp_servers]]\nlabel = \"a\"\nurl = \"https://y.test\"\n";
        assert!(AppConfig::parse(duplicate).is_err());

        let colon = "[[mcp_servers]]\nlabel = \"a:b\"\nurl = \"https://x.test\"\n";
        assert!(AppConfig::parse(colon).is_err());
    }

    #[test]
    fn missing_explicit_config_file_is_an_error() {
        assert!(AppConfig::load("definitely-missing-ano-config.toml").is_err());
        assert!(AppConfig::load_or_default("definitely-missing-ano-config.toml").is_ok());
    }

    #[test]
    fn resolves_environment_paths_relative_to_config_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "[environments.project]\nworkspace = 'repo'\n[[mcp_servers]]\nlabel = 'local'\ntransport = 'stdio'\ncommand = 'node'\ncwd = 'servers'\n").unwrap();
        let config = AppConfig::load(&path).unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        assert_eq!(
            config.environments["project"].workspace,
            Some(root.join("repo"))
        );
        assert_eq!(config.mcp_servers[0].cwd, Some(root.join("servers")));
    }

    #[test]
    fn rejects_empty_models_and_zero_budgets() {
        for text in [
            "[agent]\nmodel = '  '",
            "[agent]\nmax_output_tokens = 0",
            "[agent]\nmax_total_tokens = 0",
            "[agent]\ncompact_threshold_bytes = 1023",
            "[agent]\ncompact_threshold_bytes = 16777217",
            "[api]\ntimeout_secs = 0",
            "[webhook]\njob_timeout_secs = 0",
            "[environments.project]\nmodel = ''",
        ] {
            assert!(AppConfig::parse(text).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn rejects_webhook_paths_that_conflict_with_management_routes() {
        for path in [
            "/jobs",
            "/jobs/task/cancel",
            "/healthz",
            "/tasks/{id}",
            "/:id",
            "/tasks?q=1",
        ] {
            let text = format!("[webhook]\npath = '{path}'");
            assert!(AppConfig::parse(&text).is_err(), "accepted {path}");
        }
        assert!(AppConfig::parse("[webhook]\npath = '/hooks/tasks'").is_ok());
    }
}
