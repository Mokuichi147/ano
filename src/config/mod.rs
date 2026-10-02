//! The configuration file, which gathers the settings of every layer.
//!
//! Each layer owns its settings type; this module only combines them, loads
//! the TOML file, and resolves paths relative to it. `selection` resolves the
//! provider, model, and effort of a run from presets and other layers, and
//! `edit` rewrites the file for the `ano provider`, `ano model`, `ano preset`,
//! and `ano mcp` commands.

mod edit;
mod selection;
#[cfg(test)]
mod tests;

pub use edit::{
    remove_preset, remove_provider, rename_provider, save_mcp_tool_filters, set_role_preset,
    update_preset, update_provider, use_provider, SettingValue,
};
pub use selection::{ModelRequest, ModelSelection, PresetSettings, DEFAULT_PRESET};

use crate::{
    application::settings::validate_reasoning_effort,
    domain::{environment::EnvironmentConfig, mcp::McpServerConfig, policy::UserPolicy},
    harness::{profile::ExecutionProfile, settings::AgentConfig},
    infrastructure::{
        chronotope::HistorySettings,
        openai::{ApiSettings, ProviderSettings},
        skills::SkillSettings,
    },
    interface::webhook::WebhookSettings,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Component, Path, PathBuf},
};

/// The file name of the config file, both in the current directory and in
/// the OS config directory.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// The default `history.data_dir` before it moved to the OS data directory,
/// relative to the config file.
const LEGACY_HISTORY_DIR: &str = ".ano/history";

/// The OS config directory (macOS: `~/Library/Application Support/ano`,
/// Linux: `$XDG_CONFIG_HOME/ano` or `~/.config/ano`, Windows:
/// `%APPDATA%\ano\config`).
fn user_config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ano").map(|dirs| dirs.config_dir().to_path_buf())
}

/// `config.toml` in the OS config directory.
pub fn user_config_path() -> Option<PathBuf> {
    user_config_dir().map(|dir| dir.join(CONFIG_FILE_NAME))
}

/// `.env` in the OS config directory, read after the one of the current
/// directory, so that API keys need not be exported in every shell.
pub fn user_env_path() -> Option<PathBuf> {
    user_config_dir().map(|dir| dir.join(".env"))
}

/// The config file to read and edit when `--config` is not given:
/// `config.toml` in the current directory when it exists, otherwise the one in
/// the OS config directory, which the editing commands create when missing.
pub fn default_config_path() -> PathBuf {
    choose_config_path(PathBuf::from(CONFIG_FILE_NAME), user_config_path())
}

fn choose_config_path(local: PathBuf, user: Option<PathBuf>) -> PathBuf {
    match user {
        Some(user) if !local.exists() => user,
        _ => local,
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub api: ApiSettings,
    /// `[api]` 以外の名前付き接続先。`--provider`・environment の `provider`・
    /// `ano chat` の `/provider` で選ぶ。
    pub providers: BTreeMap<String, ProviderSettings>,
    /// 接続先・モデル・推論の強さの名前付きの組。`--preset`・environment の
    /// `preset`・`ano chat` の `/preset`・`[agent.roles]` で選ぶ。
    pub presets: BTreeMap<String, PresetSettings>,
    pub agent: AgentConfig,
    pub mcp_servers: Vec<McpServerConfig>,
    pub users: HashMap<String, UserPolicy>,
    pub environments: HashMap<String, EnvironmentConfig>,
    pub webhook: WebhookSettings,
    pub history: HistorySettings,
    pub skills: SkillSettings,
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
        for server in &mut config.mcp_servers {
            let label = &server.label;
            if let Some(url) = &mut server.url {
                *url = expand_environment_variables(url.as_str(), |name| std::env::var(name).ok())
                    .with_context(|| format!("invalid url of MCP server '{}'", label))?;
            }
        }
        config.resolve_paths(directory, std::env::home_dir().as_deref())?;
        Ok(config)
    }

    /// Expand a leading `~` to `home` and resolve other relative paths from
    /// the config file's `directory`. A stdio `command` is only expanded:
    /// bare names such as `node` are still looked up on `PATH`.
    fn resolve_paths(&mut self, directory: &Path, home: Option<&Path>) -> Result<()> {
        if let Some(path) = &mut self.api.chatgpt_auth_file {
            *path = resolve_path(path, directory, home).context("invalid api.chatgpt_auth_file")?;
        }
        for (name, provider) in &mut self.providers {
            if let Some(path) = &mut provider.chatgpt_auth_file {
                *path = resolve_path(path, directory, home)
                    .with_context(|| format!("invalid providers.{name}.chatgpt_auth_file"))?;
            }
        }
        match &mut self.history.data_dir {
            Some(dir) => *dir = resolve_path(dir, directory, home)?,
            // 以前の既定の保存先に未送信キューが残っていれば、送れるように使い続ける。
            None => {
                let legacy = directory.join(LEGACY_HISTORY_DIR);
                if legacy.is_dir() {
                    self.history.data_dir = Some(legacy);
                }
            }
        }
        if let Some(dir) = &mut self.skills.dir {
            *dir = resolve_path(dir, directory, home).context("invalid skills.dir")?;
        }
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
        self.history.validate()?;
        if self.api.timeout_secs == 0 {
            bail!("api.timeout_secs must be greater than zero");
        }
        if self.api.context_window == Some(0) {
            bail!("api.context_window must be greater than zero");
        }
        for (name, provider) in &self.providers {
            validate_provider(name, provider)
                .with_context(|| format!("invalid providers.{name}"))?;
        }
        if let Some(provider) = &self.agent.provider {
            self.provider_settings(provider)
                .context("invalid agent.provider")?;
        }
        for (name, preset) in &self.presets {
            self.validate_preset(name, preset)
                .with_context(|| format!("invalid presets.{name}"))?;
        }
        for (role, preset) in self.agent.roles.iter() {
            if let Some(preset) = preset {
                self.check_preset(preset)
                    .with_context(|| format!("invalid agent.roles.{role}"))?;
            }
        }
        for name in self.provider_names() {
            let settings = self.provider_settings(name)?;
            let table = match name {
                API_PROVIDER => API_PROVIDER.to_string(),
                _ => format!("providers.{name}"),
            };
            for rule in settings
                .disabled_models
                .iter()
                .chain(settings.allowed_models.iter().flatten())
            {
                if rule.trim().is_empty() {
                    bail!("{table} has an empty model name");
                }
                if rule.trim_end_matches('*').contains('*') {
                    bail!("{table} has the model rule '{rule}'; '*' is only allowed at the end");
                }
            }
            for model in &settings.models {
                if model.trim().is_empty() || model.contains('*') {
                    bail!("{table}.models must name models exactly; '{model}' is not a model name");
                }
            }
            for next in &settings.fallback {
                if next == name {
                    bail!("{table}.fallback must not name the provider itself");
                }
                self.provider_settings(next)
                    .with_context(|| format!("invalid {table}.fallback"))?;
            }
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
            if let Some(provider) = &environment.provider {
                self.provider_settings(provider)
                    .with_context(|| format!("invalid environments.{name}.provider"))?;
            }
            if let Some(preset) = &environment.preset {
                self.check_preset(preset)
                    .with_context(|| format!("invalid environments.{name}.preset"))?;
            }
            if let Some(effort) = &environment.reasoning_effort {
                validate_reasoning_effort(
                    &format!("environments.{name}.reasoning_effort"),
                    effort,
                )?;
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

    /// Names of every provider, `api` (`[api]`) first.
    pub fn provider_names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(API_PROVIDER).chain(self.providers.keys().map(String::as_str))
    }

    /// Names of the providers to show: `api` only while it is the default
    /// provider or something refers to it, so an unused `[api]` never
    /// appears beside the configured providers.
    pub fn listed_provider_names(&self) -> impl Iterator<Item = &str> {
        let api_in_use = self.default_provider() == API_PROVIDER
            || self
                .presets
                .values()
                .any(|preset| preset.provider.as_deref() == Some(API_PROVIDER))
            || self
                .environments
                .values()
                .any(|environment| environment.provider.as_deref() == Some(API_PROVIDER))
            || self
                .providers
                .values()
                .any(|provider| provider.fallback.iter().any(|name| name == API_PROVIDER));
        self.provider_names()
            .filter(move |name| api_in_use || *name != API_PROVIDER)
    }

    /// The connection settings of the provider `name`; `api` is `[api]`.
    pub fn provider_settings(&self, name: &str) -> Result<ApiSettings> {
        if name == API_PROVIDER {
            return Ok(self.api.clone());
        }
        match self.providers.get(name) {
            Some(provider) => Ok(provider.api_settings(&self.api)),
            None => bail!(
                "unknown provider '{name}'; choose one of: {}",
                self.provider_names().collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// Whether the provider `name` may be used. `api` always may.
    pub fn provider_enabled(&self, name: &str) -> bool {
        self.providers
            .get(name)
            .is_none_or(|provider| provider.enabled)
    }

    /// The model a switch to `name` uses by itself: `[agent].model` for
    /// `api`, the provider's `model` otherwise.
    pub fn provider_model(&self, name: &str) -> Option<&str> {
        match self.providers.get(name) {
            Some(provider) => provider.model.as_deref(),
            None => Some(&self.agent.settings.model),
        }
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

/// The name of the provider configured by `[api]`.
pub const API_PROVIDER: &str = "api";

/// Names of providers and presets: ASCII letters, digits, '-', '_', and '.'.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("names may contain only ASCII letters, digits, '-', '_', and '.'");
    }
    Ok(())
}

fn validate_provider(name: &str, provider: &ProviderSettings) -> Result<()> {
    if name == API_PROVIDER {
        bail!("'{API_PROVIDER}' is reserved for [api]");
    }
    validate_name(name).context("invalid provider name")?;
    if provider.timeout_secs == Some(0) {
        bail!("timeout_secs must be greater than zero");
    }
    if provider.context_window == Some(0) {
        bail!("context_window must be greater than zero");
    }
    for (field, value) in [
        ("model", &provider.model),
        ("approval_model", &provider.approval_model),
        ("base_url", &provider.base_url),
        ("api_key_env", &provider.api_key_env),
    ] {
        if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
            bail!("{field} must not be empty");
        }
    }
    Ok(())
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

/// Replace environment-variable references in `text` with each variable's
/// value, using `get_env` to look them up. `${VAR}` and `$VAR` are expanded
/// when their name is a valid identifier; a literal `$$` becomes one `$`. A
/// reference whose variable is unset or set to an empty string is an error, so
/// an omitted setting never silently yields an empty endpoint. Anything that is
/// not a clean `${IDENTIFIER}` (unclosed brace, dots, spaces) is left untouched.
pub fn expand_environment_variables(
    text: &str,
    get_env: impl Fn(&str) -> Option<String>,
) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '$' => match chars.peek() {
                None => out.push('$'),
                Some(&'$') => {
                    out.push('$');
                    chars.next();
                }
                Some(&'{') => {
                    chars.next();
                    let mut name = String::new();
                    let mut terminated = false;
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch == '}' {
                            terminated = true;
                            break;
                        }
                        name.push(ch);
                    }
                    if terminated && is_valid_var_name(&name) {
                        out.push_str(&lookup_env_variable(&name, &get_env)?);
                    } else {
                        // Not a clean reference: keep the whole `${...}` (or an
                        // unterminated `${`) as literal text.
                        out.push_str("${");
                        out.push_str(&name);
                        if terminated {
                            out.push('}');
                        }
                    }
                }
                Some(&first) if is_var_start(first) => {
                    let mut name = String::new();
                    while let Some(&ch) = chars.peek() {
                        if is_var_part(ch) {
                            chars.next();
                            name.push(ch);
                        } else {
                            break;
                        }
                    }
                    out.push_str(&lookup_env_variable(&name, &get_env)?);
                }
                _ => out.push('$'),
            },
            other => out.push(other),
        }
    }
    Ok(out)
}

fn lookup_env_variable<F>(name: &str, get_env: &F) -> Result<String>
where
    F: Fn(&str) -> Option<String>,
{
    match get_env(name) {
        Some(value) if !value.is_empty() => Ok(value),
        Some(_) => bail!("environment variable '{name}' is set but empty"),
        None => bail!("environment variable '{name}' is not set; the URL was not expanded"),
    }
}

fn is_var_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_var_part(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

fn is_valid_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if is_var_start(c) => chars.all(is_var_part),
        _ => false,
    }
}
