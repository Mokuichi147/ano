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
    io::Write,
    path::{Component, Path, PathBuf},
};
use toml_edit::{Array, DocumentMut, Item, TableLike, Value};

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
        config.resolve_paths(directory, std::env::home_dir().as_deref())?;
        Ok(config)
    }

    /// Expand a leading `~` to `home` and resolve other relative paths from
    /// the config file's `directory`. A stdio `command` is only expanded:
    /// bare names such as `node` are still looked up on `PATH`.
    fn resolve_paths(&mut self, directory: &Path, home: Option<&Path>) -> Result<()> {
        for (name, environment) in &mut self.environments {
            if let Some(workspace) = &mut environment.workspace {
                *workspace = resolve_path(workspace, directory, home)
                    .with_context(|| format!("invalid environments.{name}.workspace"))?;
            }
        }
        for server in &mut self.mcp_servers {
            let label = &server.label;
            if let Some(cwd) = &mut server.cwd {
                *cwd = resolve_path(cwd, directory, home)
                    .with_context(|| format!("invalid cwd of MCP server '{label}'"))?;
            }
            if let Some(command) = &mut server.command {
                if let Some(expanded) = expand_home(Path::new(command.as_str()), home)
                    .with_context(|| format!("invalid command of MCP server '{label}'"))?
                {
                    *command = expanded.to_string_lossy().into_owned();
                }
            }
        }
        Ok(())
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
            environment
                .validate()
                .with_context(|| format!("invalid environments.{name}"))?;
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

/// Save `server`'s `allowed_tools` and `disabled_tools` to its
/// `[[mcp_servers]]` entry in the config file at `path`. The rest of the
/// file, including comments and formatting, is kept as it is, and the file is
/// replaced only when the result is still a valid config.
pub fn save_mcp_tool_filters(path: &Path, server: &McpServerConfig) -> Result<()> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("failed to resolve config file {}", path.display()))?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let updated = with_mcp_tool_filters(&text, server)?;
    AppConfig::parse(&updated).context("the updated config would be invalid")?;

    let directory = path
        .parent()
        .context("config file has no parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .with_context(|| format!("failed to write config file {}", path.display()))?;
    file.write_all(updated.as_bytes())?;
    std::fs::set_permissions(file.path(), std::fs::metadata(&path)?.permissions())?;
    file.persist(&path)
        .with_context(|| format!("failed to replace config file {}", path.display()))?;
    Ok(())
}

fn with_mcp_tool_filters(text: &str, server: &McpServerConfig) -> Result<String> {
    let mut document: DocumentMut = text.parse().context("failed to parse TOML")?;
    let label = server.label.as_str();
    let has_label =
        |entry: &dyn TableLike| entry.get("label").and_then(Item::as_str) == Some(label);
    let entry: Option<&mut dyn TableLike> = match document.get_mut("mcp_servers") {
        Some(Item::ArrayOfTables(tables)) => tables
            .iter_mut()
            .map(|table| table as &mut dyn TableLike)
            .find(|entry| has_label(&**entry)),
        Some(Item::Value(Value::Array(array))) => array
            .iter_mut()
            .filter_map(|value| value.as_inline_table_mut())
            .map(|table| table as &mut dyn TableLike)
            .find(|entry| has_label(&**entry)),
        _ => None,
    };
    let entry = entry.with_context(|| format!("MCP server '{label}' is not in the config file"))?;
    set_string_list(entry, "allowed_tools", server.allowed_tools.as_deref());
    let disabled = Some(server.disabled_tools.as_slice()).filter(|names| !names.is_empty());
    set_string_list(entry, "disabled_tools", disabled);
    Ok(document.to_string())
}

/// Set `key` to `names`, or remove it for `None`. A replaced value keeps its
/// place and surrounding comments.
fn set_string_list(entry: &mut dyn TableLike, key: &str, names: Option<&[String]>) {
    let Some(names) = names else {
        entry.remove(key);
        return;
    };
    let mut array: Array = names.iter().map(String::as_str).collect();
    if let Some(Item::Value(existing)) = entry.get(key) {
        *array.decor_mut() = existing.decor().clone();
    }
    entry.insert(key, Item::Value(Value::Array(array)));
}

fn resolve_path(path: &Path, directory: &Path, home: Option<&Path>) -> Result<PathBuf> {
    Ok(match expand_home(path, home)? {
        Some(expanded) => expanded,
        None if path.is_relative() => directory.join(path),
        None => path.to_path_buf(),
    })
}

/// `~` or `~/rest` under `home`; `None` when the path does not start with `~`.
/// Other forms such as `~user` are left as they are.
fn expand_home(path: &Path, home: Option<&Path>) -> Result<Option<PathBuf>> {
    let mut components = path.components();
    if components.next() != Some(Component::Normal("~".as_ref())) {
        return Ok(None);
    }
    let home = home.context("cannot expand '~': the home directory is unknown")?;
    Ok(Some(home.join(components.as_path())))
}

#[cfg(test)]
mod tests {
    use super::{expand_home, save_mcp_tool_filters, with_mcp_tool_filters, AppConfig};
    use std::path::Path;

    #[test]
    fn saves_mcp_tool_filters_keeping_comments_and_other_servers() {
        let text = "# servers\n[[mcp_servers]]\nlabel = 'docs'\ntransport = 'stdio'\ncommand = 'node'\n\n[[mcp_servers]]\n# keep this\nlabel = 'files'\ntransport = 'stdio'\ncommand = 'node'\nallowed_tools = ['read', 'write'] # trusted\n\n[users.alice]\ndisabled_tools = ['files:write']\n";
        let mut config = AppConfig::parse(text).unwrap();
        let files = &mut config.mcp_servers[1];
        files.set_tools_enabled(["write"], false);
        files.disabled_tools.push("delete".into());

        let updated = with_mcp_tool_filters(text, files).unwrap();
        assert_eq!(
            updated,
            text.replace(
                "allowed_tools = ['read', 'write'] # trusted\n",
                "allowed_tools = [\"read\"] # trusted\ndisabled_tools = [\"delete\"]\n"
            )
        );

        files.disabled_tools.clear();
        let restored = with_mcp_tool_filters(&updated, files).unwrap();
        assert!(!restored.contains("disabled_tools = [\""));
        assert!(restored.contains("[users.alice]\ndisabled_tools = ['files:write']"));
    }

    #[test]
    fn saves_mcp_tool_filters_of_inline_tables() {
        let text = "mcp_servers = [{ label = 'files', transport = 'stdio', command = 'node' }]\n";
        let mut config = AppConfig::parse(text).unwrap();
        config.mcp_servers[0].set_tools_enabled(["delete"], false);
        let updated = with_mcp_tool_filters(text, &config.mcp_servers[0]).unwrap();
        let reloaded = AppConfig::parse(&updated).unwrap();
        assert_eq!(reloaded.mcp_servers[0].disabled_tools, ["delete"]);
    }

    #[test]
    fn saving_mcp_tool_filters_needs_the_server_in_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let text = "[[mcp_servers]]\nlabel = 'files'\ntransport = 'stdio'\ncommand = 'node'\n";
        std::fs::write(&path, text).unwrap();
        let mut server = AppConfig::parse(text).unwrap().mcp_servers.remove(0);
        server.set_tools_enabled(["delete"], false);
        save_mcp_tool_filters(&path, &server).unwrap();
        let saved = AppConfig::load(&path).unwrap();
        assert_eq!(saved.mcp_servers[0].disabled_tools, ["delete"]);

        server.label = "missing".into();
        assert!(save_mcp_tool_filters(&path, &server).is_err());
    }

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
    fn oauth_requires_streamable_http_without_a_static_token() {
        let server = |extra: &str| {
            format!("[[mcp_servers]]\nlabel = 'a'\ntransport = 'streamable_http'\nurl = 'https://x.test/mcp'\n{extra}")
        };
        assert!(AppConfig::parse(&server("oauth = true\noauth_scopes = ['read']")).is_ok());
        for text in [
            server("oauth = true\nauthorization_env = 'TOKEN'"),
            server("oauth_scopes = ['read']"),
            "[[mcp_servers]]\nlabel = 'a'\nurl = 'https://x.test/mcp'\noauth = true".into(),
        ] {
            assert!(AppConfig::parse(&text).is_err(), "accepted {text}");
        }
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
    fn expands_home_in_workspace_cwd_and_command() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = AppConfig::parse("[environments.home]\nworkspace = '~'\n[environments.project]\nworkspace = '~/repo'\n[environments.literal]\nworkspace = '~user/repo'\n[[mcp_servers]]\nlabel = 'local'\ntransport = 'stdio'\ncommand = '~/bin/server'\ncwd = '~/servers'\n[[mcp_servers]]\nlabel = 'path'\ntransport = 'stdio'\ncommand = 'node'\n").unwrap();
        let home = Path::new("/home/alice");
        config.resolve_paths(directory.path(), Some(home)).unwrap();
        let workspace = |name: &str| config.environments[name].workspace.clone().unwrap();
        assert_eq!(workspace("home"), home);
        assert_eq!(workspace("project"), home.join("repo"));
        assert_eq!(workspace("literal"), directory.path().join("~user/repo"));
        assert_eq!(config.mcp_servers[0].cwd, Some(home.join("servers")));
        assert_eq!(
            config.mcp_servers[0].command.as_deref().map(Path::new),
            Some(home.join("bin/server").as_path())
        );
        assert_eq!(config.mcp_servers[1].command.as_deref(), Some("node"));
    }

    #[test]
    fn unknown_home_is_an_error_only_when_needed() {
        assert!(expand_home(Path::new("~/servers"), None).is_err());
        assert_eq!(expand_home(Path::new("servers"), None).unwrap(), None);
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
