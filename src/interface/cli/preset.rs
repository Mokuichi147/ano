//! `ano preset`: add, change, and remove presets (named sets of a provider,
//! a model, and a reasoning effort) in the config file, choose the default
//! one, and assign presets to sub-agents and the approval reviewer.

use crate::{
    application::settings::REASONING_EFFORTS,
    config::{
        remove_preset, set_role_preset, update_preset, use_preset, AppConfig, ModelRequest,
        SettingValue, DEFAULT_PRESET,
    },
};
use anyhow::{bail, Result};
use clap::{Args, Subcommand, ValueEnum};
use std::path::Path;

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
    /// Remove a preset from the config file.
    Remove { name: String },
    /// Make a preset the default for runs that choose none ([agent].preset).
    /// `default` returns to the settings of [agent].
    Use { name: String },
    /// Choose the preset of a role ([agent.roles]). Without one, the role uses
    /// the main agent's provider, model, and effort.
    Role {
        #[arg(value_enum)]
        role: Role,
        /// The preset; `default` is the run's configured provider, model,
        /// and effort, even after `ano chat` switched away from them.
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

pub(super) fn run(config: &AppConfig, config_path: &Path, args: PresetArgs) -> Result<()> {
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
        PresetCommand::Use { name } => {
            config.check_preset(&name)?;
            config.select_model(&[ModelRequest::preset(&name)])?;
            if config.agent.preset.as_deref().unwrap_or(DEFAULT_PRESET) == name {
                println!("Preset '{name}' is already the default.");
                return Ok(());
            }
            use_preset(config_path, &name)?;
            if name == DEFAULT_PRESET {
                println!("Runs now use the settings of [agent] unless a preset is chosen.");
            } else {
                println!("Runs now use preset '{name}' unless one is chosen.");
            }
            println!("Saved to {}.", config_path.display());
            Ok(())
        }
        PresetCommand::Role {
            role,
            preset,
            unset,
        } => {
            let key = role.key();
            let preset = preset.filter(|_| !unset);
            if let Some(preset) = &preset {
                config.check_preset(preset)?;
            }
            set_role_preset(config_path, key, preset.as_deref())?;
            match preset {
                Some(preset) => println!("The {key} role now uses preset '{preset}'."),
                None => println!("The {key} role now uses the main agent's model."),
            }
            println!("Saved to {}.", config_path.display());
            Ok(())
        }
    }
}

/// Where the preset `name` is used: `[agent]`, roles, and environments.
fn users_of(config: &AppConfig, name: &str) -> Vec<String> {
    let mut users = Vec::new();
    if config.agent.preset.as_deref() == Some(name) {
        users.push("[agent].preset".to_string());
    }
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
    match &config.agent.preset {
        Some(preset) => default.push_str(&format!("  ([agent] with preset '{preset}')")),
        None => default.push_str("  ([agent])"),
    }
    let mut lines = vec![default];
    for (name, preset) in &config.presets {
        let mut line = format!("{name:width$}  {}", preset.summary());
        if config.agent.preset.as_ref() == Some(name) {
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
        .map(|(role, preset)| format!("  {role:8}  {}", preset.unwrap_or("(main agent's model)")))
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
    fn preset(path: &Path, arguments: &[&str]) -> anyhow::Result<AppConfig> {
        let mut all = vec!["ano", "--config", path.to_str().unwrap(), "preset"];
        all.extend(arguments);
        let config = AppConfig::load_or_default(path)?;
        match Cli::try_parse_from(all)?.command {
            Command::Preset(args) => super::run(&config, path, args)?,
            _ => unreachable!(),
        }
        AppConfig::load(path)
    }

    #[test]
    fn presets_and_roles_are_managed_from_the_command_line() {
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
        .unwrap();
        assert_eq!(config.presets["quick"].provider.as_deref(), Some("lan"));
        assert!(preset(&path, &["add", "quick", "--model", "x"]).is_err());
        assert!(preset(&path, &["add", "empty", "--description", "nothing"]).is_err());
        assert!(preset(&path, &["add", "bad", "--reasoning-effort", "hight"]).is_err());
        assert!(preset(&path, &["add", "default", "--model", "x"]).is_err());
        assert!(preset(&path, &["add", "far", "--provider", "missing"]).is_err());

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
        .unwrap();
        assert_eq!(config.presets["quick"].model.as_deref(), Some("qwen-4b"));
        assert_eq!(config.presets["quick"].description, None);

        let config = preset(&path, &["use", "quick"]).unwrap();
        assert_eq!(config.agent.preset.as_deref(), Some("quick"));
        assert_eq!(config.default_provider(), "lan");
        let selection = config.select_model(&[]).unwrap();
        assert_eq!(selection.choice.model, "qwen-4b");
        assert_eq!(selection.choice.reasoning_effort.as_deref(), Some("low"));

        let config = preset(&path, &["role", "review", "default"]).unwrap();
        assert_eq!(config.agent.roles.review.as_deref(), Some("default"));
        let config = preset(&path, &["role", "delegate", "quick"]).unwrap();
        assert_eq!(config.agent.roles.delegate.as_deref(), Some("quick"));
        assert!(preset(&path, &["role", "delegate", "missing"]).is_err());

        // A preset in use cannot be removed.
        assert!(preset(&path, &["remove", "quick"]).is_err());
        preset(&path, &["role", "delegate", "--unset"]).unwrap();
        let config = preset(&path, &["use", "default"]).unwrap();
        assert_eq!(config.agent.preset, None);
        let config = preset(&path, &["remove", "quick"]).unwrap();
        assert!(config.presets.is_empty());
        assert!(config
            .select_model(&[ModelRequest::preset("quick")])
            .is_err());
        preset(&path, &["list"]).unwrap();
    }
}
