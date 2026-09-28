//! `ano model`: list the models of providers, register models for providers
//! that do not list theirs, and enable or disable them in the config file,
//! like the tools of MCP servers.

use super::provider::{known_models, provider_heading, KnownModels};
use crate::{
    config::{update_provider, AppConfig, SettingValue},
    domain::provider::ModelFilter,
    infrastructure::openai::ApiSettings,
};
use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use inquire::{list_option::ListOption, InquireError, MultiSelect};
use std::{io::IsTerminal, path::Path};

#[derive(Debug, Args)]
pub(super) struct ModelArgs {
    #[command(subcommand)]
    command: ModelCommand,
}

#[derive(Debug, Subcommand)]
enum ModelCommand {
    /// List the models of providers (those they offer and those registered),
    /// marking the enabled ones.
    List {
        /// Provider to list. Omit to list every enabled provider.
        #[arg(long, value_name = "NAME")]
        provider: Option<String>,
    },
    /// Register models in the config file, for a provider that does not list
    /// its models (such as a ChatGPT subscription).
    Add(ModelsArgs),
    /// Remove registered models from the config file.
    Remove(ModelsArgs),
    /// Choose the enabled models of a provider from a checklist and save the
    /// choice to the config file.
    Edit {
        /// Provider whose models to choose. Defaults to the default provider.
        #[arg(long, value_name = "NAME")]
        provider: Option<String>,
    },
    /// Enable models of a provider in the config file.
    Enable(ToggleArgs),
    /// Disable models of a provider in the config file.
    Disable(ToggleArgs),
}

#[derive(Debug, Args)]
struct ModelsArgs {
    #[arg(required = true, value_name = "MODEL")]
    models: Vec<String>,
    /// Provider of the models. Defaults to the default provider.
    #[arg(long, value_name = "NAME")]
    provider: Option<String>,
}

#[derive(Debug, Args)]
struct ToggleArgs {
    /// Models to enable or disable: exact names, or a prefix ending in `*`.
    #[arg(required = true, value_name = "MODEL")]
    models: Vec<String>,
    /// Provider of the models. Defaults to the default provider.
    #[arg(long, value_name = "NAME")]
    provider: Option<String>,
    #[arg(
        long,
        help = "Save without connecting to the provider to check the model names"
    )]
    no_verify: bool,
}

pub(super) async fn run(config: &AppConfig, config_path: &Path, args: ModelArgs) -> Result<()> {
    match args.command {
        ModelCommand::List { provider } => {
            let names: Vec<&str> = match &provider {
                Some(name) => {
                    config.provider_settings(name)?;
                    vec![name]
                }
                None => config
                    .listed_provider_names()
                    .filter(|name| config.provider_enabled(name))
                    .collect(),
            };
            let mut failed = 0;
            for (index, name) in names.into_iter().enumerate() {
                if index > 0 {
                    println!();
                }
                let settings = config.provider_settings(name)?;
                match known_models(&settings).await {
                    Ok(known) => print_models(config, name, &settings, &known),
                    Err(error) => {
                        failed += 1;
                        println!("{}", provider_heading(name, &settings));
                        println!("  error: {error:#}");
                    }
                }
            }
            if failed > 0 {
                bail!("failed to list the models of {failed} provider(s)");
            }
            Ok(())
        }
        ModelCommand::Add(args) => register(config, config_path, args, true),
        ModelCommand::Remove(args) => register(config, config_path, args, false),
        ModelCommand::Edit { provider } => {
            let name = provider.as_deref().unwrap_or(config.default_provider());
            let settings = config.provider_settings(name)?;
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                bail!("`ano model edit` needs a terminal; use `ano model enable` or `ano model disable` instead");
            }
            let models = known_models(&settings).await?.names;
            if models.is_empty() {
                println!("Provider '{name}' has no models to choose from; register them with `ano model add MODEL --provider {name}`.");
                return Ok(());
            }
            let mut filter = settings.model_filter();
            let Some(selected) = choose_models(name, &filter, &models)? else {
                println!("Cancelled; the config file was not changed.");
                return Ok(());
            };
            let (enable, disable): (Vec<&str>, Vec<&str>) = models
                .iter()
                .map(String::as_str)
                .filter(|model| selected.contains(model) != filter.is_enabled(model))
                .partition(|model| selected.contains(model));
            if enable.is_empty() && disable.is_empty() {
                println!("No changes.");
                return Ok(());
            }
            // Enable first: disabling then only adds to or removes from lists.
            let mut blocked = filter.set_enabled(enable.iter().copied(), true);
            blocked.extend(filter.set_enabled(disable.iter().copied(), false));
            save_filter(config_path, name, &filter)?;
            for (heading, names) in [("Enabled", &enable), ("Disabled", &disable)] {
                if !names.is_empty() {
                    println!("{heading}: {}", names.join(", "));
                }
            }
            warn_blocked(&blocked);
            println!("Saved to {}.", config_path.display());
            Ok(())
        }
        ModelCommand::Enable(args) => toggle(config, config_path, args, true).await,
        ModelCommand::Disable(args) => toggle(config, config_path, args, false).await,
    }
}

/// Add models to, or remove them from, the provider's `models`.
fn register(config: &AppConfig, config_path: &Path, args: ModelsArgs, add: bool) -> Result<()> {
    let name = args
        .provider
        .as_deref()
        .unwrap_or(config.default_provider());
    let mut registered = config.provider_settings(name)?.models;
    let changed: Vec<&str> = args
        .models
        .iter()
        .map(String::as_str)
        .filter(|model| registered.iter().any(|known| known == model) != add)
        .collect();
    if changed.is_empty() {
        let state = if add {
            "already registered"
        } else {
            "not registered"
        };
        println!("{} {state} for provider '{name}'.", args.models.join(", "));
        return Ok(());
    }
    if add {
        registered.extend(changed.iter().map(|model| model.to_string()));
    } else {
        registered.retain(|model| !changed.contains(&model.as_str()));
    }
    let value = (!registered.is_empty()).then_some(SettingValue::List(registered));
    update_provider(config_path, name, false, &[("models", value)])?;
    println!(
        "{} for provider '{name}': {}\nSaved to {}.",
        if add { "Registered" } else { "Removed" },
        changed.join(", "),
        config_path.display()
    );
    Ok(())
}

async fn toggle(
    config: &AppConfig,
    config_path: &Path,
    args: ToggleArgs,
    enabled: bool,
) -> Result<()> {
    let name = args
        .provider
        .as_deref()
        .unwrap_or(config.default_provider());
    let settings = config.provider_settings(name)?;
    let state = if enabled { "enabled" } else { "disabled" };
    let exact: Vec<&str> = args
        .models
        .iter()
        .map(String::as_str)
        .filter(|model| !model.ends_with('*'))
        .collect();
    if !args.no_verify && !exact.is_empty() {
        let known = known_models(&settings)
            .await
            .context("failed to check the model names (use --no-verify to skip the check)")?;
        let unknown: Vec<&str> = exact
            .iter()
            .copied()
            .filter(|model| !known.names.iter().any(|known| known == model))
            .collect();
        if !unknown.is_empty() {
            let models = unknown.join(" ");
            match known.listed {
                Some(_) => bail!(
                    "provider '{name}' does not offer {}; run `ano model list --provider {name}` to see its models, or register them with `ano model add {models} --provider {name}`",
                    unknown.join(", ")
                ),
                None => bail!(
                    "provider '{name}' does not list its models, and {} is not registered; register it with `ano model add {models} --provider {name}`",
                    unknown.join(", ")
                ),
            }
        }
    }
    let mut filter = settings.model_filter();
    let changed: Vec<&str> = args
        .models
        .iter()
        .map(String::as_str)
        .filter(|model| filter.is_enabled(model) != enabled || model.ends_with('*'))
        .collect();
    if changed.is_empty() {
        println!(
            "Already {state} for provider '{name}': {}",
            args.models.join(", ")
        );
        return Ok(());
    }
    let blocked = filter.set_enabled(changed.iter().copied(), enabled);
    save_filter(config_path, name, &filter)?;
    println!(
        "{} for provider '{name}': {}",
        if enabled { "Enabled" } else { "Disabled" },
        changed.join(", ")
    );
    warn_blocked(&blocked);
    println!("Saved to {}.", config_path.display());
    Ok(())
}

fn save_filter(config_path: &Path, name: &str, filter: &ModelFilter) -> Result<()> {
    update_provider(
        config_path,
        name,
        false,
        &[
            (
                "allowed_models",
                filter.allowed.clone().map(SettingValue::List),
            ),
            (
                "disabled_models",
                (!filter.disabled.is_empty()).then(|| SettingValue::List(filter.disabled.clone())),
            ),
        ],
    )
}

fn warn_blocked(blocked: &[&str]) {
    if !blocked.is_empty() {
        eprintln!(
            "warning: a pattern in allowed_models or disabled_models still decides {}; change the pattern with `ano provider set PROVIDER --unset allowed-models` or `--unset disabled-models`",
            blocked.join(", ")
        );
    }
}

fn print_models(config: &AppConfig, name: &str, settings: &ApiSettings, known: &KnownModels) {
    let filter = settings.model_filter();
    let models = &known.names;
    let enabled = models
        .iter()
        .filter(|model| filter.is_enabled(model))
        .count();
    let kind = if known.listed.is_some() {
        "models"
    } else {
        "registered models"
    };
    let mut heading = format!(
        "{}: {} {kind}, {enabled} enabled",
        provider_heading(name, settings),
        models.len()
    );
    if !config.provider_enabled(name) {
        heading.push_str(" (the provider is disabled)");
    }
    println!("{heading}");
    if known.listed.is_none() && models.is_empty() {
        println!("  The provider does not list its models; register the ones to use with `ano model add MODEL --provider {name}`.");
    }
    let default = config.provider_model(name);
    for model in models {
        let mark = if filter.is_enabled(model) {
            "[x]"
        } else {
            "[ ]"
        };
        let mut line = format!("  {mark} {model}");
        if Some(model.as_str()) == default {
            line.push_str("  (default)");
        }
        if known.only_registered(model) {
            line.push_str("  (registered; not listed by the provider)");
        }
        println!("{line}");
    }
    let unknown: Vec<&str> = filter
        .allowed
        .iter()
        .flatten()
        .chain(&filter.disabled)
        .map(String::as_str)
        .filter(|rule| !rule.ends_with('*') && !models.iter().any(|model| model == rule))
        .collect();
    if !unknown.is_empty() {
        println!(
            "  Names in the config that are neither listed nor registered: {}",
            unknown.join(", ")
        );
    }
}

/// Show a checklist of `models` with the enabled ones checked. Returns the
/// checked models, or `None` when the user cancels.
fn choose_models<'a>(
    name: &str,
    filter: &ModelFilter,
    models: &'a [String],
) -> Result<Option<Vec<&'a str>>> {
    let defaults: Vec<usize> = models
        .iter()
        .enumerate()
        .filter(|(_, model)| filter.is_enabled(model))
        .map(|(index, _)| index)
        .collect();
    let choices: Vec<&str> = models.iter().map(String::as_str).collect();
    let message = format!("Models enabled for provider '{name}'");
    let summary =
        |checked: &[ListOption<&&str>]| format!("{} of {} models", checked.len(), models.len());
    let answer = MultiSelect::new(&message, choices)
        .with_default(&defaults)
        .with_formatter(&summary)
        .with_page_size(15)
        .with_help_message(
            "↑↓ move, space toggle, → all, ← none, type to filter, enter save, esc cancel",
        )
        .prompt();
    match answer {
        Ok(selected) => Ok(Some(selected)),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
