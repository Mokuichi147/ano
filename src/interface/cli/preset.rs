//! `ano preset`: add, change, and remove presets (named sets of a provider,
//! a model, and a reasoning effort) in the config file, and assign them to
//! the main agent, sub-agents, and the approval reviewer.

use super::provider::known_models;
use crate::{
    application::settings::REASONING_EFFORTS,
    config::{
        remove_preset, set_role_preset, update_preset, AppConfig, ModelRequest, PresetSettings,
        SettingValue, API_PROVIDER, DEFAULT_PRESET,
    },
};
use anyhow::{bail, Result};
use clap::{Args, Subcommand, ValueEnum};
use inquire::{InquireError, Select, Text};
use std::{io::IsTerminal, path::Path};

#[derive(Debug, Args)]
pub(super) struct PresetArgs {
    #[command(subcommand)]
    command: PresetCommand,
}

#[derive(Debug, Subcommand)]
enum PresetCommand {
    /// List the presets, the default one, and the presets of the roles.
    List,
    /// Add a preset to the config file.
    Add {
        name: String,
        #[command(flatten)]
        options: PresetOptions,
    },
    /// Change the settings of a preset.
    Set {
        name: String,
        #[command(flatten)]
        options: PresetOptions,
        /// Remove a setting, so the preset keeps what lower layers chose.
        #[arg(long, value_enum, value_name = "FIELD")]
        unset: Vec<Field>,
    },
    /// Change a preset by choosing its provider, model, and effort from lists.
    Edit {
        /// The preset; chosen from a list when omitted.
        name: Option<String>,
    },
    /// Remove a preset from the config file.
    Remove { name: String },
    /// Choose the preset of a role ([agent.roles]). `default` is the main
    /// agent's for runs that choose none; the other roles without one use the
    /// main agent's provider, model, and effort.
    Role {
        #[arg(value_enum)]
        role: Role,
        /// The preset. For the default role, `default` is the settings of
        /// [agent]; for the others, it is the run's configured provider,
        /// model, and effort, even after `ano chat` switched away from them.
        #[arg(required_unless_present = "unset")]
        preset: Option<String>,
        /// Remove the role's preset.
        #[arg(long, conflicts_with = "preset")]
        unset: bool,
    },
}

#[derive(Debug, Default, Args)]
struct PresetOptions {
    /// Provider from [providers] ('api' is [api]).
    #[arg(long, value_name = "NAME")]
    provider: Option<String>,
    /// Model on the provider.
    #[arg(long)]
    model: Option<String>,
    /// Reasoning effort: none, minimal, low, medium, high, xhigh, max, or ultra.
    #[arg(long, value_name = "LEVEL")]
    reasoning_effort: Option<String>,
    /// What the preset is for, shown in listings.
    #[arg(long, value_name = "TEXT")]
    description: Option<String>,
}

/// A setting that `--unset` removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Field {
    Provider,
    Model,
    ReasoningEffort,
    Description,
}

impl Field {
    fn key(self) -> &'static str {
        match self {
            Field::Provider => "provider",
            Field::Model => "model",
            Field::ReasoningEffort => "reasoning_effort",
            Field::Description => "description",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Role {
    /// The main agent, for runs that choose no preset.
    Default,
    /// Sub-agents started by delegate_task.
    Delegate,
    /// The reviewer started by review_changes.
    Review,
    /// The reviewer of approval_mode = "auto".
    Approval,
}

impl Role {
    fn key(self) -> &'static str {
        match self {
            Role::Default => "default",
            Role::Delegate => "delegate",
            Role::Review => "review",
            Role::Approval => "approval",
        }
    }
}

impl PresetOptions {
    /// The keys to write, in config file order.
    fn changes(&self) -> Result<Vec<(&'static str, Option<SettingValue>)>> {
        if let Some(effort) = &self.reasoning_effort {
            if !REASONING_EFFORTS.contains(&effort.as_str()) {
                bail!(
                    "--reasoning-effort must be one of {}",
                    REASONING_EFFORTS.join(", ")
                );
            }
        }
        Ok([
            ("provider", &self.provider),
            ("model", &self.model),
            ("reasoning_effort", &self.reasoning_effort),
            ("description", &self.description),
        ]
        .into_iter()
        .filter_map(|(key, value)| {
            value
                .clone()
                .map(|value| (key, Some(SettingValue::Text(value))))
        })
        .collect())
    }
}

pub(super) async fn run(config: &AppConfig, config_path: &Path, args: PresetArgs) -> Result<()> {
    match args.command {
        PresetCommand::List => {
            println!("{}", format_presets(config));
            Ok(())
        }
        PresetCommand::Add { name, options } => {
            let changes = options.changes()?;
            if changes.iter().all(|(key, _)| *key == "description") {
                bail!("a preset needs --provider, --model, or --reasoning-effort");
            }
            update_preset(config_path, &name, true, &changes)?;
            println!("Added preset '{name}' to {}.", config_path.display());
            warn_if_unusable(config_path, &name);
            Ok(())
        }
        PresetCommand::Set {
            name,
            options,
            unset,
        } => {
            let mut changes = options.changes()?;
            if let Some(field) = unset
                .iter()
                .find(|field| changes.iter().any(|(key, _)| *key == field.key()))
            {
                bail!("--unset {} conflicts with a new value for it", field.key());
            }
            changes.extend(unset.iter().map(|field| (field.key(), None)));
            if changes.is_empty() {
                bail!("nothing to change; pass settings such as --model or --reasoning-effort, or --unset FIELD");
            }
            update_preset(config_path, &name, false, &changes)?;
            println!("Updated preset '{name}' in {}.", config_path.display());
            warn_if_unusable(config_path, &name);
            Ok(())
        }
        PresetCommand::Remove { name } => {
            if name == DEFAULT_PRESET {
                bail!("'{DEFAULT_PRESET}' is the settings of [agent] and cannot be removed");
            }
            if let Some(user) = users_of(config, &name).first() {
                bail!("preset '{name}' is used by {user}; choose another there first");
            }
            remove_preset(config_path, &name)?;
            println!("Removed preset '{name}' from {}.", config_path.display());
            Ok(())
        }
        PresetCommand::Edit { name } => edit(config, config_path, name).await,
        PresetCommand::Role {
            role,
            preset,
            unset,
        } => {
            let key = role.key();
            // The default role's `default` is [agent] itself: no preset.
            let preset = preset
                .filter(|_| !unset)
                .filter(|preset| role != Role::Default || preset != DEFAULT_PRESET);
            if let Some(preset) = &preset {
                config.check_preset(preset)?;
                if role == Role::Default {
                    config.select_model(&[ModelRequest::preset(preset)])?;
                }
            }
            set_role_preset(config_path, key, preset.as_deref())?;
            match (preset, role) {
                (Some(preset), Role::Default) => {
                    println!("Runs now use preset '{preset}' unless one is chosen.")
                }
                (Some(preset), _) => println!("The {key} role now uses preset '{preset}'."),
                (None, Role::Default) => {
                    println!("Runs now use the settings of [agent] unless a preset is chosen.")
                }
                (None, _) => println!("The {key} role now uses the main agent's model."),
            }
            println!("Saved to {}.", config_path.display());
            Ok(())
        }
    }
}

/// `Ok(None)` when the user cancels a prompt.
fn answered<T>(answer: Result<T, InquireError>) -> Result<Option<T>> {
    match answer {
        Ok(value) => Ok(Some(value)),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Ask for one of `values`, with `unset` (the label of leaving the field out)
/// first and the cursor on `current`. `None` when the user cancels;
/// `Some(None)` for `unset`.
fn choose(
    message: &str,
    unset: &str,
    values: &[String],
    current: Option<&str>,
) -> Result<Option<Option<String>>> {
    let mut options = vec![unset.to_string()];
    options.extend(values.iter().cloned());
    let cursor = current
        .and_then(|current| values.iter().position(|value| value == current))
        .map_or(0, |index| index + 1);
    let answer = Select::new(message, options)
        .with_starting_cursor(cursor)
        .with_page_size(15)
        .with_help_message("↑↓ move, type to filter, enter choose, esc cancel")
        .raw_prompt();
    Ok(answered(answer)?.map(|chosen| (chosen.index > 0).then_some(chosen.value)))
}

/// `ano preset edit`: choose the provider, the model among those the
/// provider offers, the effort, and the description, then save what changed.
async fn edit(config: &AppConfig, config_path: &Path, name: Option<String>) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("`ano preset edit` needs a terminal; use `ano preset set` instead");
    }
    let cancelled = || {
        println!("Cancelled; the config file was not changed.");
        Ok(())
    };
    let name = match name {
        Some(name) => name,
        None => {
            let names: Vec<String> = config.presets.keys().cloned().collect();
            if names.is_empty() {
                println!("There are no presets yet; add one with `ano preset add NAME`.");
                return Ok(());
            }
            match answered(Select::new("Preset to edit", names).prompt())? {
                Some(name) => name,
                None => return cancelled(),
            }
        }
    };
    if name == DEFAULT_PRESET {
        bail!("'{DEFAULT_PRESET}' is the settings of [agent]; choose a preset for runs with `ano preset role default NAME`");
    }
    let Some(current) = config.presets.get(&name) else {
        bail!("preset '{name}' is not in the config file; add it with `ano preset add {name}`");
    };

    // Those `ano provider list` shows (`api` only while in use), and the current one.
    let providers: Vec<String> = config
        .provider_names()
        .filter(|provider| {
            let listed = config.listed_provider_names().any(|name| name == *provider)
                && config.provider_enabled(provider);
            listed || current.provider.as_deref() == Some(*provider)
        })
        .map(str::to_string)
        .collect();
    let Some(provider) = choose(
        &format!("Provider of preset '{name}'"),
        "(not set: keep the provider chosen under the preset)",
        &providers,
        current.provider.as_deref(),
    )?
    else {
        return cancelled();
    };

    // The models the chosen provider offers and enables, and the current one.
    // Without a provider of its own, the preset usually runs on that of
    // [agent], so its models are offered.
    let listed = provider
        .clone()
        .or_else(|| config.agent.provider.clone())
        .unwrap_or_else(|| API_PROVIDER.to_string());
    let unset_model = match config
        .provider_model(&listed)
        .filter(|_| provider.is_some())
    {
        Some(model) => format!("(not set: the provider's model, {model})"),
        None => "(not set: keep the model chosen under the preset)".to_string(),
    };
    let settings = config.provider_settings(&listed)?;
    let mut models = match known_models(&settings).await {
        Ok(known) => known.names,
        Err(error) => {
            eprintln!("warning: could not list the models of '{listed}': {error:#}");
            settings.models.clone()
        }
    };
    let filter = settings.model_filter();
    models.retain(|model| filter.is_enabled(model));
    if let Some(model) = &current.model {
        if !models.contains(model) {
            models.insert(0, model.clone());
        }
    }
    const OTHER_MODEL: &str = "(enter another model name)";
    models.push(OTHER_MODEL.to_string());
    let Some(mut model) = choose(
        &format!("Model (listed from provider '{listed}')"),
        &unset_model,
        &models,
        current.model.as_deref(),
    )?
    else {
        return cancelled();
    };
    if model.as_deref() == Some(OTHER_MODEL) {
        let answer = Text::new("Model name")
            .with_initial_value(current.model.as_deref().unwrap_or_default())
            .prompt();
        match answered(answer)? {
            Some(typed) if !typed.trim().is_empty() => model = Some(typed.trim().to_string()),
            Some(_) => model = None,
            None => return cancelled(),
        }
    }

    let efforts: Vec<String> = REASONING_EFFORTS
        .iter()
        .map(|effort| effort.to_string())
        .collect();
    let Some(reasoning_effort) = choose(
        "Reasoning effort (which ones work depends on the model)",
        "(not set: keep the effort chosen under the preset)",
        &efforts,
        current.reasoning_effort.as_deref(),
    )?
    else {
        return cancelled();
    };

    let answer = Text::new("Description")
        .with_initial_value(current.description.as_deref().unwrap_or_default())
        .with_help_message("shown in listings; leave empty for none")
        .prompt();
    let Some(description) = answered(answer)? else {
        return cancelled();
    };
    let description = Some(description.trim().to_string()).filter(|text| !text.is_empty());

    let edited = PresetSettings {
        provider,
        model,
        reasoning_effort,
        description,
    };
    let changes = preset_changes(current, &edited);
    if changes.is_empty() {
        println!("No changes.");
        return Ok(());
    }
    update_preset(config_path, &name, false, &changes)?;
    println!(
        "Updated preset '{name}': {}\nSaved to {}.",
        edited.summary(),
        config_path.display()
    );
    warn_if_unusable(config_path, &name);
    Ok(())
}

/// The keys of `before` that `after` changes, with their new values (`None`
/// removes a key).
fn preset_changes(
    before: &PresetSettings,
    after: &PresetSettings,
) -> Vec<(&'static str, Option<SettingValue>)> {
    [
        ("provider", &before.provider, &after.provider),
        ("model", &before.model, &after.model),
        (
            "reasoning_effort",
            &before.reasoning_effort,
            &after.reasoning_effort,
        ),
        ("description", &before.description, &after.description),
    ]
    .into_iter()
    .filter(|(_, before, after)| before != after)
    .map(|(key, _, after)| (key, after.clone().map(SettingValue::Text)))
    .collect()
}

/// Where the preset `name` is used: roles and environments.
fn users_of(config: &AppConfig, name: &str) -> Vec<String> {
    let mut users = Vec::new();
    for (role, preset) in config.agent.roles.iter() {
        if preset == Some(name) {
            users.push(format!("agent.roles.{role}"));
        }
    }
    let mut environments: Vec<&String> = config
        .environments
        .iter()
        .filter(|(_, environment)| environment.preset.as_deref() == Some(name))
        .map(|(environment, _)| environment)
        .collect();
    environments.sort();
    users.extend(
        environments
            .into_iter()
            .map(|environment| format!("environments.{environment}.preset")),
    );
    users
}

/// Warn when the saved preset cannot be used as it is, for example because
/// its model is disabled. It is kept: the model may be enabled later.
fn warn_if_unusable(config_path: &Path, name: &str) {
    let result = AppConfig::load(config_path)
        .and_then(|config| config.select_model(&[ModelRequest::preset(name)]));
    if let Err(error) = result {
        eprintln!("warning: preset '{name}' cannot be used yet: {error:#}");
    }
}

fn format_presets(config: &AppConfig) -> String {
    let names: Vec<&str> = std::iter::once(DEFAULT_PRESET)
        .chain(config.presets.keys().map(String::as_str))
        .collect();
    let width = names.iter().map(|name| name.len()).max().unwrap_or(0);
    // `default` is what runs use unless they choose: [agent] with its preset.
    let mut default = match config.select_model(&[]) {
        Ok(selection) => format!("{DEFAULT_PRESET:width$}  {}", selection.describe()),
        Err(error) => format!("{DEFAULT_PRESET:width$}  (unusable: {error:#})"),
    };
    match &config.agent.roles.default {
        Some(preset) => default.push_str(&format!("  ([agent] with preset '{preset}')")),
        None => default.push_str("  ([agent])"),
    }
    let mut lines = vec![default];
    for (name, preset) in &config.presets {
        let mut line = format!("{name:width$}  {}", preset.summary());
        if config.agent.roles.default.as_ref() == Some(name) {
            line.push_str("  (default)");
        }
        if let Some(description) = &preset.description {
            line.push_str(&format!("\n{:width$}  {description}", ""));
        }
        lines.push(line);
    }
    let roles: Vec<String> = config
        .agent
        .roles
        .iter()
        .map(|(role, preset)| {
            let unset = if role == "default" {
                "([agent])"
            } else {
                "(main agent's model)"
            };
            format!("  {role:8}  {}", preset.unwrap_or(unset))
        })
        .collect();
    lines.push(format!("\nroles:\n{}", roles.join("\n")));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::super::{Cli, Command};
    use crate::config::{AppConfig, ModelRequest};
    use clap::Parser;
    use std::path::Path;

    /// Run `ano --config PATH preset ARGUMENTS...` and load the config it leaves.
    async fn preset(path: &Path, arguments: &[&str]) -> anyhow::Result<AppConfig> {
        let mut all = vec!["ano", "--config", path.to_str().unwrap(), "preset"];
        all.extend(arguments);
        let config = AppConfig::load_or_default(path)?;
        match Cli::try_parse_from(all)?.command {
            Command::Preset(args) => super::run(&config, path, args).await?,
            _ => unreachable!(),
        }
        AppConfig::load(path)
    }

    #[test]
    fn edits_save_only_the_changed_fields() {
        use super::preset_changes;
        use crate::config::{PresetSettings, SettingValue};
        let text = |value: &str| Some(value.to_string());
        let before = PresetSettings {
            provider: text("lan"),
            model: text("qwen"),
            reasoning_effort: text("low"),
            description: text("Surveys"),
        };
        assert!(preset_changes(&before, &before).is_empty());
        let after = PresetSettings {
            provider: None,
            reasoning_effort: text("ultra"),
            description: None,
            ..before.clone()
        };
        assert_eq!(
            preset_changes(&before, &after),
            [
                ("provider", None),
                ("reasoning_effort", Some(SettingValue::Text("ultra".into()))),
                ("description", None),
            ]
        );
    }

    #[tokio::test]
    async fn presets_and_roles_are_managed_from_the_command_line() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            "[agent]\nmodel = 'gpt-main'\n[providers.lan]\nbase_url = 'http://127.0.0.1:9/v1'\nmodel = 'qwen'\n",
        )
        .unwrap();

        let config = preset(
            &path,
            &[
                "add",
                "quick",
                "--provider",
                "lan",
                "--reasoning-effort",
                "low",
                "--description",
                "Surveys",
            ],
        )
        .await
        .unwrap();
        assert_eq!(config.presets["quick"].provider.as_deref(), Some("lan"));
        assert!(preset(&path, &["add", "quick", "--model", "x"])
            .await
            .is_err());
        assert!(preset(&path, &["add", "empty", "--description", "nothing"])
            .await
            .is_err());
        assert!(
            preset(&path, &["add", "bad", "--reasoning-effort", "hight"])
                .await
                .is_err()
        );
        assert!(preset(&path, &["add", "default", "--model", "x"])
            .await
            .is_err());
        assert!(preset(&path, &["add", "far", "--provider", "missing"])
            .await
            .is_err());

        let config = preset(
            &path,
            &[
                "set",
                "quick",
                "--model",
                "qwen-4b",
                "--unset",
                "description",
            ],
        )
        .await
        .unwrap();
        assert_eq!(config.presets["quick"].model.as_deref(), Some("qwen-4b"));
        assert_eq!(config.presets["quick"].description, None);

        let config = preset(&path, &["role", "default", "quick"]).await.unwrap();
        assert_eq!(config.agent.roles.default.as_deref(), Some("quick"));
        assert_eq!(config.default_provider(), "lan");
        let selection = config.select_model(&[]).unwrap();
        assert_eq!(selection.choice.model, "qwen-4b");
        assert_eq!(selection.choice.reasoning_effort.as_deref(), Some("low"));

        let config = preset(&path, &["role", "review", "default"]).await.unwrap();
        assert_eq!(config.agent.roles.review.as_deref(), Some("default"));
        let config = preset(&path, &["role", "delegate", "quick"]).await.unwrap();
        assert_eq!(config.agent.roles.delegate.as_deref(), Some("quick"));
        assert!(preset(&path, &["role", "delegate", "missing"])
            .await
            .is_err());

        // A preset in use cannot be removed.
        assert!(preset(&path, &["remove", "quick"]).await.is_err());
        preset(&path, &["role", "delegate", "--unset"])
            .await
            .unwrap();
        // The default role's `default` is [agent] without a preset.
        let config = preset(&path, &["role", "default", "default"])
            .await
            .unwrap();
        assert_eq!(config.agent.roles.default, None);
        assert!(
            preset(&path, &["edit", "quick"]).await.is_err(),
            "needs a terminal"
        );
        let config = preset(&path, &["remove", "quick"]).await.unwrap();
        assert!(config.presets.is_empty());
        assert!(config
            .select_model(&[ModelRequest::preset("quick")])
            .is_err());
        preset(&path, &["list"]).await.unwrap();
    }
}
