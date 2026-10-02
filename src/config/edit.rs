//! Changes to the config file that keep its comments and formatting, for
//! the commands that manage providers, models, presets, and MCP tools.

use super::{AppConfig, API_PROVIDER, DEFAULT_PRESET};
use crate::domain::mcp::McpServerConfig;
use anyhow::{bail, Context, Result};
use std::{io::Write, path::Path};
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value};

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

/// Write `changes` to the preset `name` in the config file at `path`,
/// creating the file when it does not exist. `None` removes a key. `adding`
/// requires a new preset; otherwise it must exist.
pub fn update_preset(
    path: &Path,
    name: &str,
    adding: bool,
    changes: &[(&str, Option<SettingValue>)],
) -> Result<()> {
    edit_config_file(path, true, |text| {
        if name == DEFAULT_PRESET {
            bail!("'{DEFAULT_PRESET}' is the settings of [agent]; change them with `ano provider set` or in the config file");
        }
        let mut document: DocumentMut = text.parse().context("failed to parse TOML")?;
        set_entry_values(&mut document, "presets", name, adding, changes)?;
        Ok(document.to_string())
    })
}

/// Remove the preset `name` from the config file at `path`. Refused while
/// a role or an environment uses it.
pub fn remove_preset(path: &Path, name: &str) -> Result<()> {
    edit_config_file(path, false, |text| {
        let mut document: DocumentMut = text.parse().context("failed to parse TOML")?;
        let removed = document
            .get_mut("presets")
            .and_then(Item::as_table_like_mut)
            .and_then(|presets| presets.remove(name));
        if removed.is_none() {
            bail!("preset '{name}' is not in the config file");
        }
        Ok(document.to_string())
    })
}

/// Set the preset of `role` in `[agent.roles]` of the config file at `path`,
/// or remove it for `None`.
pub fn set_role_preset(path: &Path, role: &str, preset: Option<&str>) -> Result<()> {
    edit_config_file(path, true, |text| {
        let mut document: DocumentMut = text.parse().context("failed to parse TOML")?;
        let agent = table_mut(&mut document, "agent")?;
        let roles = agent
            .entry("roles")
            .or_insert(Item::Table(Table::new()))
            .as_table_like_mut()
            .context("agent.roles is not a table")?;
        let value = preset.map(|name| SettingValue::Text(name.into()));
        set_value(roles, role, value.as_ref());
        if roles.is_empty() {
            agent.remove("roles");
        }
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
    "wire_api",
    "models",
    "allowed_models",
    "disabled_models",
    "fallback",
    "context_window",
];

pub(super) fn with_provider_renamed(text: &str, old: &str, new: &str) -> Result<String> {
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
    for section in ["environments", "providers", "presets"] {
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
    if text.is_none() {
        // The config names commands and workspaces, so a directory created for
        // it is the owner's only.
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(directory)
            .with_context(|| format!("failed to create directory {}", directory.display()))?;
    }
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

pub(super) fn with_provider_changes(
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
    set_entry_values(&mut document, "providers", name, adding, changes)?;
    Ok(document.to_string())
}

/// Apply `changes` to the table `[SECTION.NAME]` (`providers` or `presets`).
/// `adding` requires a new entry; otherwise it must exist.
fn set_entry_values(
    document: &mut DocumentMut,
    section: &str,
    name: &str,
    adding: bool,
    changes: &[(&str, Option<SettingValue>)],
) -> Result<()> {
    // `providers` -> `provider`, `presets` -> `preset`
    let kind = section.trim_end_matches('s');
    let exists = document
        .get(section)
        .and_then(Item::as_table_like)
        .is_some_and(|entries| entries.contains_key(name));
    match (adding, exists) {
        (true, true) => {
            bail!("{kind} '{name}' already exists; change it with `ano {kind} set {name}`")
        }
        (false, false) => bail!("{kind} '{name}' is not in the config file"),
        _ => {}
    }
    let entries = document.entry(section).or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    let entry = entries
        .as_table_like_mut()
        .with_context(|| format!("{section} is not a table"))?
        .entry(name)
        .or_insert(Item::Table(Table::new()))
        .as_table_like_mut()
        .with_context(|| format!("{section}.{name} is not a table"))?;
    for (key, value) in changes {
        set_value(entry, key, value.as_ref());
    }
    Ok(())
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

pub(super) fn with_mcp_tool_filters(text: &str, server: &McpServerConfig) -> Result<String> {
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
