//! The configuration file, which gathers the settings of every layer.
//!
//! Each layer owns its settings type; this module only combines them, loads
//! the TOML file, and resolves paths relative to it.

use crate::{
    application::{profile::ExecutionProfile, settings::AgentSettings},
    domain::session::ModelChoice,
    domain::{environment::EnvironmentConfig, mcp::McpServerConfig, policy::UserPolicy},
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
    io::Write,
    path::{Component, Path, PathBuf},
};
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub api: ApiSettings,
    /// `[api]` 以外の名前付き接続先。`--provider`・environment の `provider`・
    /// `ano chat` の `/provider` で選ぶ。
    pub providers: BTreeMap<String, ProviderSettings>,
    pub agent: AgentSettings,
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
        self.history.data_dir = resolve_path(&self.history.data_dir, directory, home)?;
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
        for (name, provider) in &self.providers {
            validate_provider(name, provider)
                .with_context(|| format!("invalid providers.{name}"))?;
        }
        if let Some(provider) = &self.agent.provider {
            self.provider_settings(provider)
                .context("invalid agent.provider")?;
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

    /// The provider runs use unless one is chosen: `[agent].provider`, or
    /// `api` without it.
    pub fn default_provider(&self) -> &str {
        self.agent.provider.as_deref().unwrap_or(API_PROVIDER)
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
            None => Some(&self.agent.model),
        }
    }

    /// Resolve the provider and model of a run from `requests`, lowest
    /// precedence first (for example the environment, then the command line).
    ///
    /// The default provider comes first, with its model. A request that
    /// names a model uses it. A request that names only a provider switches
    /// to that provider's model (`[agent].model` for `api`) when it has one,
    /// and otherwise keeps the model from lower layers.
    pub fn select_model(&self, requests: &[ModelRequest]) -> Result<ModelSelection> {
        let default = ModelRequest {
            provider: Some(self.default_provider().to_string()),
            model: None,
        };
        let mut provider = API_PROVIDER.to_string();
        let mut model = self.agent.model.clone();
        let mut approval_model = self.agent.approval_model.clone();
        for request in std::iter::once(&default).chain(requests) {
            if let Some(name) = &request.provider {
                self.provider_settings(name)?;
                provider.clone_from(name);
                if let Some(default) = self.provider_model(name) {
                    model = default.to_string();
                }
                approval_model = match self.providers.get(name) {
                    Some(settings) => settings.approval_model.clone(),
                    None => self.agent.approval_model.clone(),
                };
            }
            if let Some(name) = &request.model {
                if name.trim().is_empty() {
                    bail!("model must not be empty");
                }
                model.clone_from(name);
            }
        }
        if !self.provider_enabled(&provider) {
            bail!("provider '{provider}' is disabled; enable it with `ano provider enable {provider}`");
        }
        let api = self.provider_settings(&provider)?;
        if !api.model_filter().is_enabled(&model) {
            bail!("model '{model}' is disabled for provider '{provider}'; enable it with `ano model enable {model} --provider {provider}` or choose another model");
        }
        Ok(ModelSelection {
            api,
            choice: ModelChoice { provider, model },
            approval_model,
        })
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

/// One layer of a provider and model choice, such as an environment or the
/// command line. Unset fields leave the lower layers in effect.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelRequest {
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl ModelRequest {
    pub fn is_empty(&self) -> bool {
        self.provider.is_none() && self.model.is_none()
    }
}

impl From<&ModelChoice> for ModelRequest {
    fn from(choice: &ModelChoice) -> Self {
        Self {
            provider: Some(choice.provider.clone()),
            model: Some(choice.model.clone()),
        }
    }
}

/// The provider and model a run uses, with the provider's connection.
#[derive(Debug, Clone)]
pub struct ModelSelection {
    pub choice: ModelChoice,
    pub api: ApiSettings,
    /// Reviewer model for `approval_mode = "auto"`; `None` uses the model.
    pub approval_model: Option<String>,
}

impl ModelSelection {
    /// Apply the model to `settings` of a run.
    pub fn apply_to(&self, settings: &mut AgentSettings) {
        settings.model.clone_from(&self.choice.model);
        settings.approval_model.clone_from(&self.approval_model);
    }
}

fn validate_provider(name: &str, provider: &ProviderSettings) -> Result<()> {
    if name == API_PROVIDER {
        bail!("'{API_PROVIDER}' is reserved for [api]");
    }
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("provider names may contain only ASCII letters, digits, '-', '_', and '.'");
    }
    if provider.timeout_secs == Some(0) {
        bail!("timeout_secs must be greater than zero");
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

/// Save `server`'s `allowed_tools` and `disabled_tools` to its
/// `[[mcp_servers]]` entry in the config file at `path`. The rest of the
/// file, including comments and formatting, is kept as it is, and the file is
/// replaced only when the result is still a valid config.
pub fn save_mcp_tool_filters(path: &Path, server: &McpServerConfig) -> Result<()> {
    edit_config_file(path, false, |text| with_mcp_tool_filters(text, server))
}

/// A value to write to the config file.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Text(String),
    Integer(i64),
    Bool(bool),
    List(Vec<String>),
}

/// Write `changes` to the settings of the provider `name` in the config file
/// at `path`, creating the file when it does not exist. `None` removes a key.
/// `adding` requires a new provider; otherwise it must exist. For `default`,
/// `model` and `approval_model` go to `[agent]` and the rest to `[api]`.
///
/// Comments and formatting are kept, and the file is replaced only when the
/// result is still a valid config.
pub fn update_provider(
    path: &Path,
    name: &str,
    adding: bool,
    changes: &[(&str, Option<SettingValue>)],
) -> Result<()> {
    edit_config_file(path, true, |text| {
        with_provider_changes(text, name, adding, changes)
    })
}

/// Make `name` the default provider in the config file at `path`
/// (`[agent].provider`; removed for `api`).
pub fn use_provider(path: &Path, name: &str) -> Result<()> {
    edit_config_file(path, true, |text| {
        let mut document: DocumentMut = text.parse().context("failed to parse TOML")?;
        let value = (name != API_PROVIDER).then(|| SettingValue::Text(name.into()));
        set_value(
            table_mut(&mut document, "agent")?,
            "provider",
            value.as_ref(),
        );
        Ok(document.to_string())
    })
}

/// Rename the provider `old` to `new` in the config file at `path`, with the
/// references to it: `[agent].provider`, environments, and fallback lists.
///
/// Renaming `api` moves the connection of `[api]` (and its models, filters,
/// and fallbacks) to `[providers.NEW]`, with `[agent].model` as its model and
/// the reviewer of `[agent].approval_model`. The shared `timeout_secs`,
/// `max_retries`, and `stream` stay in `[api]`. When `api` was the default,
/// the new provider becomes the default.
pub fn rename_provider(path: &Path, old: &str, new: &str) -> Result<()> {
    edit_config_file(path, false, |text| with_provider_renamed(text, old, new))
}

/// Settings of `[api]` that describe its connection rather than transport
/// defaults shared by every provider.
const API_CONNECTION_KEYS: &[&str] = &[
    "auth",
    "chatgpt_auth_file",
    "base_url",
    "api_key_env",
    "models",
    "allowed_models",
    "disabled_models",
    "fallback",
];

fn with_provider_renamed(text: &str, old: &str, new: &str) -> Result<String> {
    if new == API_PROVIDER {
        bail!("'{API_PROVIDER}' is reserved for [api]");
    }
    let mut document: DocumentMut = text.parse().context("failed to parse TOML")?;
    let exists = |document: &DocumentMut, name: &str| {
        document
            .get("providers")
            .and_then(Item::as_table_like)
            .is_some_and(|providers| providers.contains_key(name))
    };
    if exists(&document, new) {
        bail!("provider '{new}' already exists");
    }
    let moved = if old == API_PROVIDER {
        let mut table = Table::new();
        match document.get_mut("api") {
            // Moving the keys with their decor keeps the comments above them.
            Some(Item::Table(api)) => {
                for key in API_CONNECTION_KEYS {
                    if let Some((key, item)) = api.remove_entry(key) {
                        table.insert_formatted(&key, item);
                    }
                }
            }
            Some(item) => {
                if let Some(api) = item.as_table_like_mut() {
                    for key in API_CONNECTION_KEYS {
                        if let Some(item) = api.remove(key) {
                            table.insert(key, item);
                        }
                    }
                }
            }
            None => {}
        }
        if let Some(agent) = document.get_mut("agent").and_then(Item::as_table_like_mut) {
            if let Some(model) = agent.get("model") {
                table.insert("model", model.clone());
            }
            if let Some(reviewer) = agent.remove("approval_model") {
                table.insert("approval_model", reviewer);
            }
        }
        if document
            .get("agent")
            .and_then(|agent| agent.get("provider"))
            .is_none()
        {
            let agent = table_mut(&mut document, "agent")?;
            set_value(agent, "provider", Some(&SettingValue::Text(new.into())));
        }
        Item::Table(table)
    } else {
        if !exists(&document, old) {
            bail!("provider '{old}' is not in the config file");
        }
        document["providers"]
            .as_table_like_mut()
            .and_then(|providers| providers.remove(old))
            .context("providers is not a table")?
    };
    let providers = document.entry("providers").or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    providers
        .as_table_like_mut()
        .context("providers is not a table")?
        .insert(new, moved);

    let renamed = |table: &mut dyn TableLike, key: &str| {
        if let Some(Item::Value(Value::String(name))) = table.get_mut(key) {
            if name.value() == old {
                let decor = name.decor().clone();
                *name = toml_edit::Formatted::new(new.to_string());
                *name.decor_mut() = decor;
            }
        }
        if let Some(Item::Value(Value::Array(names))) = table.get_mut(key) {
            for name in names.iter_mut() {
                if name.as_str() == Some(old) {
                    let decor = name.decor().clone();
                    *name = Value::from(new);
                    *name.decor_mut() = decor;
                }
            }
        }
    };
    if let Some(agent) = document.get_mut("agent").and_then(Item::as_table_like_mut) {
        renamed(agent, "provider");
    }
    if let Some(api) = document.get_mut("api").and_then(Item::as_table_like_mut) {
        renamed(api, "fallback");
    }
    for section in ["environments", "providers"] {
        if let Some(tables) = document.get_mut(section).and_then(Item::as_table_like_mut) {
            for (_, table) in tables.iter_mut() {
                if let Some(table) = table.as_table_like_mut() {
                    renamed(table, "provider");
                    renamed(table, "fallback");
                }
            }
        }
    }
    Ok(document.to_string())
}

/// The table `name` of `document`, created when missing.
fn table_mut<'a>(document: &'a mut DocumentMut, name: &str) -> Result<&'a mut dyn TableLike> {
    document
        .entry(name)
        .or_insert(Item::Table(Table::new()))
        .as_table_like_mut()
        .with_context(|| format!("[{name}] is not a table"))
}

/// Remove the provider `name` from the config file at `path`. Refused while
/// an environment or a fallback list names it.
pub fn remove_provider(path: &Path, name: &str) -> Result<()> {
    edit_config_file(path, false, |text| {
        let mut document: DocumentMut = text.parse().context("failed to parse TOML")?;
        let removed = document
            .get_mut("providers")
            .and_then(Item::as_table_like_mut)
            .and_then(|providers| providers.remove(name));
        if removed.is_none() {
            bail!("provider '{name}' is not in the config file");
        }
        Ok(document.to_string())
    })
}

/// Replace the config file at `path` with `edit` applied to its text, when
/// the result is a valid config. With `create`, a missing file is edited as
/// an empty one and created.
fn edit_config_file(
    path: &Path,
    create: bool,
    edit: impl FnOnce(&str) -> Result<String>,
) -> Result<()> {
    let (path, text) = match std::fs::canonicalize(path) {
        Ok(path) => {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read config file {}", path.display()))?;
            (path, Some(text))
        }
        Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
            (std::path::absolute(path)?, None)
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to resolve config file {}", path.display()))
        }
    };
    let updated = edit(text.as_deref().unwrap_or_default())?;
    AppConfig::parse(&updated).context("the updated config would be invalid")?;

    let directory = path
        .parent()
        .context("config file has no parent directory")?;
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .with_context(|| format!("failed to write config file {}", path.display()))?;
    file.write_all(updated.as_bytes())?;
    if text.is_some() {
        std::fs::set_permissions(file.path(), std::fs::metadata(&path)?.permissions())?;
    }
    file.persist(&path)
        .with_context(|| format!("failed to replace config file {}", path.display()))?;
    Ok(())
}

fn with_provider_changes(
    text: &str,
    name: &str,
    adding: bool,
    changes: &[(&str, Option<SettingValue>)],
) -> Result<String> {
    let mut document: DocumentMut = text.parse().context("failed to parse TOML")?;
    if name == API_PROVIDER {
        if adding {
            bail!("'{API_PROVIDER}' is [api]; change it with `ano provider set {API_PROVIDER}`, or give it a name with `ano provider rename {API_PROVIDER} NAME`");
        }
        for (key, value) in changes {
            let section = match *key {
                "model" | "approval_model" => "agent",
                "enabled" => bail!("the '{API_PROVIDER}' provider cannot be disabled"),
                _ => "api",
            };
            let table = document
                .entry(section)
                .or_insert(Item::Table(Table::new()))
                .as_table_like_mut()
                .with_context(|| format!("[{section}] is not a table"))?;
            set_value(table, key, value.as_ref());
        }
        return Ok(document.to_string());
    }
    let exists = document
        .get("providers")
        .and_then(Item::as_table_like)
        .is_some_and(|providers| providers.contains_key(name));
    match (adding, exists) {
        (true, true) => {
            bail!("provider '{name}' already exists; change it with `ano provider set {name}`")
        }
        (false, false) => bail!("provider '{name}' is not in the config file"),
        _ => {}
    }
    let providers = document.entry("providers").or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    let provider = providers
        .as_table_like_mut()
        .context("providers is not a table")?
        .entry(name)
        .or_insert(Item::Table(Table::new()))
        .as_table_like_mut()
        .with_context(|| format!("providers.{name} is not a table"))?;
    for (key, value) in changes {
        set_value(provider, key, value.as_ref());
    }
    Ok(document.to_string())
}

/// Set `key` to `value`, or remove it for `None`. A replaced value keeps its
/// place and surrounding comments.
fn set_value(entry: &mut dyn TableLike, key: &str, value: Option<&SettingValue>) {
    let Some(value) = value else {
        entry.remove(key);
        return;
    };
    let mut value = match value {
        SettingValue::Text(text) => Value::from(text.as_str()),
        SettingValue::Integer(number) => Value::from(*number),
        SettingValue::Bool(flag) => Value::from(*flag),
        SettingValue::List(names) => {
            Value::Array(names.iter().map(String::as_str).collect::<Array>())
        }
    };
    if let Some(Item::Value(existing)) = entry.get(key) {
        *value.decor_mut() = existing.decor().clone();
    }
    entry.insert(key, Item::Value(value));
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

fn set_string_list(entry: &mut dyn TableLike, key: &str, names: Option<&[String]>) {
    set_value(
        entry,
        key,
        names
            .map(|names| SettingValue::List(names.to_vec()))
            .as_ref(),
    );
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
    use super::{
        expand_home, remove_provider, save_mcp_tool_filters, update_provider,
        with_mcp_tool_filters, with_provider_changes, with_provider_renamed, AppConfig,
        ModelRequest, SettingValue,
    };
    use std::path::Path;

    const PROVIDERS: &str = "[agent]\nmodel = 'gpt-main'\napproval_model = 'gpt-mini'\n[api]\ntimeout_secs = 30\n[providers.local]\nbase_url = 'http://192.168.1.10:1234/v1'\nmodel = 'qwen/qwen3'\n[providers.bare]\nbase_url = 'http://127.0.0.1:8000/v1'\ntimeout_secs = 5\n";

    fn request(provider: Option<&str>, model: Option<&str>) -> ModelRequest {
        ModelRequest {
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn a_provider_brings_its_model_and_later_layers_override_it() {
        let config = AppConfig::parse(PROVIDERS).unwrap();
        let chosen = |requests: &[ModelRequest]| {
            let selection = config.select_model(requests).unwrap();
            (
                selection.choice.provider,
                selection.choice.model,
                selection.approval_model,
            )
        };
        let owned = |p: &str, m: &str, a: Option<&str>| (p.into(), m.into(), a.map(Into::into));
        assert_eq!(chosen(&[]), owned("api", "gpt-main", Some("gpt-mini")));
        // The reviewer of [agent] belongs to [api]; another provider reviews
        // with its own model unless it names one.
        assert_eq!(
            chosen(&[request(Some("local"), None)]),
            owned("local", "qwen/qwen3", None)
        );
        assert_eq!(
            chosen(&[
                request(Some("local"), None),
                request(None, Some("qwen3:30b"))
            ]),
            owned("local", "qwen3:30b", None)
        );
        // A provider without a model keeps the model of lower layers.
        assert_eq!(
            chosen(&[
                request(None, Some("env-model")),
                request(Some("bare"), None)
            ]),
            owned("bare", "env-model", None)
        );
        assert_eq!(
            chosen(&[request(Some("local"), None), request(Some("api"), None)]),
            owned("api", "gpt-main", Some("gpt-mini"))
        );
        assert!(config
            .select_model(&[request(Some("missing"), None)])
            .is_err());
        assert!(config.select_model(&[request(None, Some(" "))]).is_err());
    }

    #[test]
    fn providers_inherit_transport_settings_but_not_the_endpoint() {
        let config = AppConfig::parse(PROVIDERS).unwrap();
        let local = config.provider_settings("local").unwrap();
        assert_eq!(local.base_url, "http://192.168.1.10:1234/v1");
        assert_eq!(local.timeout_secs, 30);
        assert_eq!(local.api_key_env, "OPENAI_API_KEY");
        // OPENAI_BASE_URL only redirects [api].
        assert!(!local.use_base_url_env);
        assert!(config.provider_settings("api").unwrap().use_base_url_env);
        assert_eq!(config.provider_settings("bare").unwrap().timeout_secs, 5);
        assert_eq!(
            config.provider_names().collect::<Vec<_>>(),
            ["api", "bare", "local"]
        );
    }

    #[test]
    fn provider_names_and_references_are_validated() {
        for text in [
            "[providers.api]\nbase_url = 'http://127.0.0.1:1/v1'",
            "[agent]\nprovider = 'missing'",
            "[providers.'a b']\nbase_url = 'http://127.0.0.1:1/v1'",
            "[providers.local]\nmodel = ''",
            "[providers.local]\ntimeout_secs = 0",
            "[providers.local]\nunknown = 1",
            "[environments.dev]\nprovider = 'missing'",
            "[environments.dev]\nprovider = ''",
        ] {
            assert!(AppConfig::parse(text).is_err(), "{text}");
        }
        assert!(
            AppConfig::parse("[providers.local]\n[environments.dev]\nprovider = 'local'").is_ok()
        );
        assert!(AppConfig::parse("[environments.dev]\nprovider = 'api'").is_ok());
    }

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

    #[test]
    fn disabled_providers_and_models_cannot_be_selected() {
        let config = AppConfig::parse("[agent]\nmodel = 'gpt-main'\n[api]\ndisabled_models = ['gpt-old*']\n[providers.lan]\nenabled = false\nmodel = 'qwen'\n[providers.box]\nallowed_models = ['llama*']\nmodel = 'llama3'").unwrap();
        let error = config
            .select_model(&[request(Some("lan"), None)])
            .unwrap_err();
        assert!(
            error.to_string().contains("ano provider enable lan"),
            "{error}"
        );
        let error = config
            .select_model(&[request(None, Some("gpt-old-1"))])
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("ano model enable gpt-old-1 --provider api"),
            "{error}"
        );
        assert!(config
            .select_model(&[request(Some("box"), Some("qwen"))])
            .is_err());
        assert_eq!(
            config
                .select_model(&[request(Some("box"), None)])
                .unwrap()
                .choice
                .model,
            "llama3"
        );
        assert!(!config.provider_enabled("lan"));
        assert!(config.provider_enabled("api"));
    }

    #[test]
    fn fallbacks_must_name_other_known_providers() {
        assert!(
            AppConfig::parse("[api]\nfallback = ['lan']\n[providers.lan]\nfallback = ['api']")
                .is_ok()
        );
        for text in [
            "[api]\nfallback = ['missing']",
            "[providers.lan]\nfallback = ['lan']",
            "[providers.lan]\ndisabled_models = ['']",
            "[providers.lan]\nallowed_models = ['qwen/*-4b']",
        ] {
            assert!(AppConfig::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn provider_changes_keep_the_rest_of_the_file() {
        let text = "# main endpoint\n[api]\nbase_url = 'http://127.0.0.1:1234/v1' # LM Studio\n\n[agent]\nmodel = 'qwen'\n";
        let text_value = |value: &str| Some(SettingValue::Text(value.into()));
        let added = with_provider_changes(
            text,
            "lan",
            true,
            &[
                ("base_url", text_value("http://192.168.1.10:1234/v1")),
                ("fallback", Some(SettingValue::List(vec!["api".into()]))),
            ],
        )
        .unwrap();
        assert!(added.starts_with(text), "{added}");
        assert!(
            added.contains(
                "[providers.lan]\nbase_url = \"http://192.168.1.10:1234/v1\"\nfallback = [\"api\"]"
            ),
            "{added}"
        );
        let config = AppConfig::parse(&added).unwrap();
        assert_eq!(config.providers["lan"].fallback, ["api"]);
        assert!(with_provider_changes(&added, "lan", true, &[]).is_err());
        assert!(with_provider_changes(text, "missing", false, &[]).is_err());

        let changed = with_provider_changes(
            &added,
            "lan",
            false,
            &[
                ("fallback", None),
                ("enabled", Some(SettingValue::Bool(false))),
            ],
        )
        .unwrap();
        let config = AppConfig::parse(&changed).unwrap();
        assert!(config.providers["lan"].fallback.is_empty());
        assert!(!config.provider_enabled("lan"));

        // `default` is [api], and its model is [agent].model.
        let default = with_provider_changes(
            text,
            "api",
            false,
            &[
                ("base_url", text_value("http://127.0.0.1:11434/v1")),
                ("model", text_value("llama3")),
            ],
        )
        .unwrap();
        assert!(
            default.contains("base_url = \"http://127.0.0.1:11434/v1\" # LM Studio"),
            "{default}"
        );
        assert!(default.contains("[agent]\nmodel = \"llama3\""), "{default}");
        assert!(with_provider_changes(text, "api", false, &[("enabled", None)]).is_err());
        assert!(with_provider_changes(text, "api", true, &[]).is_err());
    }

    #[test]
    fn provider_files_are_created_and_references_block_removal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        update_provider(
            &path,
            "lan",
            true,
            &[("model", Some(SettingValue::Text("qwen".into())))],
        )
        .unwrap();
        let config = AppConfig::load(&path).unwrap();
        assert_eq!(config.provider_model("lan"), Some("qwen"));
        // An invalid result is never written.
        assert!(update_provider(
            &path,
            "lan",
            false,
            &[("timeout_secs", Some(SettingValue::Integer(0)))]
        )
        .is_err());
        update_provider(
            &path,
            "api",
            false,
            &[("fallback", Some(SettingValue::List(vec!["lan".into()])))],
        )
        .unwrap();
        let error = remove_provider(&path, "lan").unwrap_err();
        assert!(format!("{error:#}").contains("fallback"), "{error:#}");
        update_provider(&path, "api", false, &[("fallback", None)]).unwrap();
        remove_provider(&path, "lan").unwrap();
        assert!(AppConfig::load(&path).unwrap().providers.is_empty());
        assert!(remove_provider(&path, "lan").is_err());
    }

    #[test]
    fn the_default_provider_brings_its_model_and_hides_an_unused_api() {
        let text = "[agent]\nmodel = 'gpt-main'\nprovider = 'lan'\n[providers.lan]\nmodel = 'qwen'\napproval_model = 'qwen-mini'\n[providers.bare]\n";
        let config = AppConfig::parse(text).unwrap();
        assert_eq!(config.default_provider(), "lan");
        let selection = config.select_model(&[]).unwrap();
        assert_eq!(selection.choice.provider, "lan");
        assert_eq!(selection.choice.model, "qwen");
        assert_eq!(selection.approval_model.as_deref(), Some("qwen-mini"));
        // A default provider without a model uses [agent].model.
        let bare =
            AppConfig::parse(&text.replace("provider = 'lan'", "provider = 'bare'")).unwrap();
        assert_eq!(bare.select_model(&[]).unwrap().choice.model, "gpt-main");

        assert_eq!(
            config.listed_provider_names().collect::<Vec<_>>(),
            ["bare", "lan"]
        );
        // `api` is still selectable, and listed while something uses it.
        assert_eq!(
            config
                .select_model(&[request(Some("api"), None)])
                .unwrap()
                .choice
                .model,
            "gpt-main"
        );
        let referenced = AppConfig::parse(&format!("{text}fallback = ['api']")).unwrap();
        assert_eq!(
            referenced.listed_provider_names().collect::<Vec<_>>(),
            ["api", "bare", "lan"]
        );
        assert_eq!(
            AppConfig::default()
                .listed_provider_names()
                .collect::<Vec<_>>(),
            ["api"]
        );
    }

    #[test]
    fn renaming_api_moves_its_connection_to_a_named_provider() {
        let text = "[api]\n# the subscription\nauth = 'chatgpt' # subscription\nmodels = ['gpt-5.6-luna']\ntimeout_secs = 30\n\n[agent]\nmodel = 'gpt-5.6-luna'\napproval_model = 'gpt-5.6-mini'\n\n[providers.lan]\nbase_url = 'http://192.168.1.10:1234/v1'\nfallback = ['api']\n\n[environments.review]\nprovider = 'api'\n";
        let renamed = with_provider_renamed(text, "api", "chatgpt").unwrap();
        let config = AppConfig::parse(&renamed).unwrap();
        assert!(
            renamed.contains("auth = 'chatgpt' # subscription"),
            "{renamed}"
        );
        let chatgpt = &config.providers["chatgpt"];
        assert_eq!(
            chatgpt.auth,
            crate::infrastructure::openai::ApiAuth::Chatgpt
        );
        assert_eq!(chatgpt.models, ["gpt-5.6-luna"]);
        assert_eq!(chatgpt.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(chatgpt.approval_model.as_deref(), Some("gpt-5.6-mini"));
        // Shared transport settings stay, and every reference follows.
        assert_eq!(config.api.timeout_secs, 30);
        assert_eq!(
            config.api.auth,
            crate::infrastructure::openai::ApiAuth::ApiKey
        );
        assert_eq!(config.default_provider(), "chatgpt");
        assert_eq!(config.providers["lan"].fallback, ["chatgpt"]);
        assert_eq!(
            config.environments["review"].provider.as_deref(),
            Some("chatgpt")
        );
        assert!(!config.listed_provider_names().any(|name| name == "api"));

        let again = with_provider_renamed(&renamed, "chatgpt", "subscription").unwrap();
        let config = AppConfig::parse(&again).unwrap();
        assert_eq!(config.default_provider(), "subscription");
        assert_eq!(config.providers["lan"].fallback, ["subscription"]);
        assert!(with_provider_renamed(&again, "subscription", "lan").is_err());
        assert!(with_provider_renamed(&again, "subscription", "api").is_err());
        assert!(with_provider_renamed(&again, "missing", "other").is_err());
    }
}
