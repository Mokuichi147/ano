//! `ano provider`: add, change, and remove providers in the config file, and
//! enable or disable them. Their models are managed by `ano model`.

use crate::{
    config::{
        remove_provider, rename_provider, update_provider, use_provider, AppConfig, SettingValue,
        API_PROVIDER,
    },
    infrastructure::openai::{create_client, ApiAuth, ApiSettings, WireApi},
};
use anyhow::{bail, Result};
use clap::{Args, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub(super) struct ProviderArgs {
    #[command(subcommand)]
    command: ProviderCommand,
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    /// List the providers with their endpoints, models, and fallbacks,
    /// marking the enabled ones.
    List,
    /// Add a provider to the config file.
    Add {
        name: String,
        #[command(flatten)]
        options: ProviderOptions,
    },
    /// Change the settings of a provider. `api` changes [api], and its model
    /// is [agent].model.
    Set {
        name: String,
        #[command(flatten)]
        options: ProviderOptions,
        /// Remove a setting, returning it to its default.
        #[arg(long, value_enum, value_name = "FIELD")]
        unset: Vec<Field>,
    },
    /// Remove a provider from the config file.
    Remove { name: String },
    /// Make a provider the default for runs that choose none
    /// ([agent].provider). `api` returns to [api].
    Use { name: String },
    /// Rename a provider and the references to it. Renaming `api` moves the
    /// connection of [api] to [providers.NEW].
    Rename { old: String, new: String },
    /// Enable a provider in the config file.
    Enable { name: String },
    /// Disable a provider in the config file, keeping its settings.
    Disable { name: String },
}

#[derive(Debug, Default, Args)]
struct ProviderOptions {
    /// API endpoint, such as http://127.0.0.1:1234/v1.
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,
    #[arg(long, value_enum)]
    auth: Option<AuthArg>,
    /// API the endpoint speaks: `responses` (POST /responses, the default)
    /// or `chat-completions` (POST /chat/completions) for servers without
    /// the Responses API.
    #[arg(long, value_enum, value_name = "API")]
    wire_api: Option<WireApiArg>,
    /// Environment variable that holds the API key.
    #[arg(long, value_name = "ENV")]
    api_key_env: Option<String>,
    /// Where the ChatGPT login is saved, for `--auth chatgpt`.
    #[arg(long, value_name = "PATH")]
    chatgpt_auth_file: Option<PathBuf>,
    /// Model to use when switching to this provider.
    #[arg(long)]
    model: Option<String>,
    /// Reviewer model for `approval_mode = "auto"` on this provider.
    #[arg(long, value_name = "MODEL")]
    approval_model: Option<String>,
    /// Providers to try in order while this one is unavailable. Replaces the
    /// current list; separate names with commas or repeat the option.
    #[arg(long, value_name = "PROVIDER", value_delimiter = ',')]
    fallback: Vec<String>,
    #[arg(long, value_name = "SECONDS")]
    timeout_secs: Option<u64>,
    #[arg(long, value_name = "N")]
    max_retries: Option<u32>,
    #[arg(long, value_name = "BOOL")]
    stream: Option<bool>,
    /// Tokens the provider's models take in one request; the history is
    /// compacted before it grows near this. LM Studio reports it by itself.
    #[arg(long, value_name = "TOKENS")]
    context_window: Option<u64>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AuthArg {
    ApiKey,
    Chatgpt,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum WireApiArg {
    Responses,
    ChatCompletions,
}

/// A setting that `--unset` removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Field {
    BaseUrl,
    Auth,
    WireApi,
    ApiKeyEnv,
    ChatgptAuthFile,
    Model,
    ApprovalModel,
    Fallback,
    TimeoutSecs,
    MaxRetries,
    Stream,
    AllowedModels,
    DisabledModels,
    ContextWindow,
}

impl Field {
    fn key(self) -> &'static str {
        match self {
            Field::BaseUrl => "base_url",
            Field::Auth => "auth",
            Field::WireApi => "wire_api",
            Field::ApiKeyEnv => "api_key_env",
            Field::ChatgptAuthFile => "chatgpt_auth_file",
            Field::Model => "model",
            Field::ApprovalModel => "approval_model",
            Field::Fallback => "fallback",
            Field::TimeoutSecs => "timeout_secs",
            Field::MaxRetries => "max_retries",
            Field::Stream => "stream",
            Field::AllowedModels => "allowed_models",
            Field::DisabledModels => "disabled_models",
            Field::ContextWindow => "context_window",
        }
    }
}

impl ProviderOptions {
    /// The keys to write, in config file order.
    fn changes(&self) -> Result<Vec<(&'static str, Option<SettingValue>)>> {
        let text = |value: &Option<String>| value.clone().map(SettingValue::Text);
        let mut changes = Vec::new();
        let mut push = |key: &'static str, value: Option<SettingValue>| {
            if let Some(value) = value {
                changes.push((key, Some(value)));
            }
        };
        push(
            "auth",
            self.auth.map(|auth| {
                SettingValue::Text(
                    match auth {
                        AuthArg::ApiKey => "api_key",
                        AuthArg::Chatgpt => "chatgpt",
                    }
                    .into(),
                )
            }),
        );
        push("base_url", text(&self.base_url));
        push(
            "wire_api",
            self.wire_api.map(|api| {
                SettingValue::Text(
                    match api {
                        WireApiArg::Responses => "responses",
                        WireApiArg::ChatCompletions => "chat_completions",
                    }
                    .into(),
                )
            }),
        );
        push("api_key_env", text(&self.api_key_env));
        push(
            "chatgpt_auth_file",
            // Relative paths in the config resolve from its directory, so
            // save the path the user meant from here.
            match &self.chatgpt_auth_file {
                Some(path) => Some(SettingValue::Text(
                    std::path::absolute(path)?.to_string_lossy().into_owned(),
                )),
                None => None,
            },
        );
        push("model", text(&self.model));
        push("approval_model", text(&self.approval_model));
        push(
            "fallback",
            (!self.fallback.is_empty()).then(|| SettingValue::List(self.fallback.clone())),
        );
        push(
            "timeout_secs",
            self.timeout_secs
                .map(|value| SettingValue::Integer(value.try_into().unwrap_or(i64::MAX))),
        );
        push(
            "max_retries",
            self.max_retries
                .map(|value| SettingValue::Integer(value.into())),
        );
        push("stream", self.stream.map(SettingValue::Bool));
        push(
            "context_window",
            self.context_window
                .map(|value| SettingValue::Integer(value.try_into().unwrap_or(i64::MAX))),
        );
        Ok(changes)
    }
}

pub(super) async fn run(config: &AppConfig, config_path: &Path, args: ProviderArgs) -> Result<()> {
    match args.command {
        ProviderCommand::List => {
            println!("{}", format_providers(config));
            Ok(())
        }
        ProviderCommand::Add { name, options } => {
            update_provider(config_path, &name, true, &options.changes()?)?;
            println!("Added provider '{name}' to {}.", config_path.display());
            report_models(config_path, &name).await;
            Ok(())
        }
        ProviderCommand::Set {
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
                bail!("nothing to change; pass settings such as --base-url or --model, or --unset FIELD");
            }
            update_provider(config_path, &name, false, &changes)?;
            println!("Updated provider '{name}' in {}.", config_path.display());
            Ok(())
        }
        ProviderCommand::Remove { name } => {
            if name == API_PROVIDER {
                bail!("the '{API_PROVIDER}' provider is [api] and cannot be removed");
            }
            remove_provider(config_path, &name)?;
            println!("Removed provider '{name}' from {}.", config_path.display());
            Ok(())
        }
        ProviderCommand::Use { name } => {
            config.provider_settings(&name)?;
            if let Some((preset, provider)) =
                config.agent.roles.default.as_ref().and_then(|preset| {
                    let provider = config.presets.get(preset)?.provider.as_ref()?;
                    Some((preset, provider))
                })
            {
                bail!("the default preset '{preset}' chooses provider '{provider}'; choose another with `ano preset role default NAME` (`default` for [agent]) first");
            }
            if !config.provider_enabled(&name) {
                bail!("provider '{name}' is disabled; enable it with `ano provider enable {name}` first");
            }
            if config.default_provider() == name {
                println!("Provider '{name}' is already the default.");
                return Ok(());
            }
            use_provider(config_path, &name)?;
            println!(
                "Runs now use provider '{name}' unless one is chosen.\nSaved to {}.",
                config_path.display()
            );
            Ok(())
        }
        ProviderCommand::Rename { old, new } => {
            config.provider_settings(&old)?;
            rename_provider(config_path, &old, &new)?;
            println!(
                "Renamed provider '{old}' to '{new}'.\nSaved to {}.",
                config_path.display()
            );
            Ok(())
        }
        ProviderCommand::Enable { name } => toggle(config, config_path, &name, true),
        ProviderCommand::Disable { name } => toggle(config, config_path, &name, false),
    }
}

fn toggle(config: &AppConfig, config_path: &Path, name: &str, enabled: bool) -> Result<()> {
    config.provider_settings(name)?;
    if name == API_PROVIDER {
        if enabled {
            println!("The '{API_PROVIDER}' provider is always enabled.");
            return Ok(());
        }
        bail!("the '{API_PROVIDER}' provider is [api] and cannot be disabled; disable its models with `ano model disable` instead");
    }
    if !enabled && config.default_provider() == name {
        match &config.agent.roles.default {
            Some(preset)
                if config
                    .presets
                    .get(preset)
                    .is_some_and(|preset| preset.provider.is_some()) =>
            {
                bail!(
                "provider '{name}' is the default through preset '{preset}'; choose another with `ano preset role default NAME` first"
            )
            }
            _ => bail!(
                "provider '{name}' is the default; choose another with `ano provider use NAME` first"
            ),
        }
    }
    if config.provider_enabled(name) == enabled {
        let state = if enabled { "enabled" } else { "disabled" };
        println!("Provider '{name}' is already {state}.");
        return Ok(());
    }
    // Enabled is the default, so enabling removes the key.
    let value = (!enabled).then_some(SettingValue::Bool(false));
    update_provider(config_path, name, false, &[("enabled", value)])?;
    println!(
        "{} provider '{name}'.\nSaved to {}.",
        if enabled { "Enabled" } else { "Disabled" },
        config_path.display()
    );
    Ok(())
}

/// The models of a provider: those it lists and those registered in the
/// config, which are all there is for a provider that lists none.
pub(super) struct KnownModels {
    /// Listed and registered models, sorted by name.
    pub(super) names: Vec<String>,
    /// What the provider listed; `None` when it does not list its models.
    pub(super) listed: Option<Vec<String>>,
}

impl KnownModels {
    pub(super) fn new(listed: Option<Vec<String>>, registered: &[String]) -> Self {
        let mut names: Vec<String> = listed.iter().flatten().chain(registered).cloned().collect();
        names.sort();
        names.dedup();
        Self { names, listed }
    }

    /// Whether `model` is known only because the config registers it,
    /// although the provider lists its models.
    pub(super) fn only_registered(&self, model: &str) -> bool {
        self.listed
            .as_ref()
            .is_some_and(|listed| !listed.iter().any(|listed| listed == model))
    }
}

pub(super) async fn known_models(settings: &ApiSettings) -> Result<KnownModels> {
    let listed = create_client(settings)?.list_models().await?;
    Ok(KnownModels::new(listed, &settings.models))
}

/// After adding a provider, show whether it answers. Failing to connect does
/// not undo the addition: the server may simply not be running yet.
async fn report_models(config_path: &Path, name: &str) {
    let result = async {
        let config = AppConfig::load(config_path)?;
        let settings = config.provider_settings(name)?;
        anyhow::Ok((
            known_models(&settings).await?,
            config.provider_model(name).map(str::to_string),
        ))
    }
    .await;
    match result {
        Ok((known, model)) => {
            match &known.listed {
                Some(listed) => println!(
                    "The provider offers {} models; see them with `ano model list --provider {name}`.",
                    listed.len()
                ),
                None => println!(
                    "The provider does not list its models; register the ones to use with `ano model add MODEL --provider {name}`."
                ),
            }
            if let Some(model) = model.filter(|model| known.only_registered(model)) {
                eprintln!("warning: the provider does not list the model '{model}'");
            }
        }
        Err(error) => eprintln!("warning: could not list the models of '{name}': {error:#}"),
    }
}

/// Where the provider connects: its URL, or the ChatGPT subscription.
pub(super) fn endpoint(settings: &ApiSettings) -> String {
    match settings.auth {
        ApiAuth::Chatgpt => "ChatGPT subscription".into(),
        ApiAuth::ApiKey => match settings.wire_api {
            WireApi::Responses => settings.effective_base_url(),
            WireApi::ChatCompletions => {
                format!("{} (chat completions)", settings.effective_base_url())
            }
        },
    }
}

pub(super) fn provider_heading(name: &str, settings: &ApiSettings) -> String {
    format!("{name} ({})", endpoint(settings))
}

fn format_providers(config: &AppConfig) -> String {
    let names: Vec<&str> = config.listed_provider_names().collect();
    let width = names.iter().map(|name| name.len()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for name in names {
        let Ok(settings) = config.provider_settings(name) else {
            continue;
        };
        let mark = if config.provider_enabled(name) {
            "[x]"
        } else {
            "[ ]"
        };
        let mut line = format!("{mark} {name:width$}  {}", endpoint(&settings));
        if let Some(model) = config.provider_model(name) {
            line.push_str(&format!("  model {model}"));
        }
        if config.default_provider() == name {
            line.push_str("  (default)");
        }
        lines.push(line);
        if let Some(allowed) = &settings.allowed_models {
            lines.push(format!("    allowed models: {}", allowed.join(", ")));
        }
        if !settings.disabled_models.is_empty() {
            lines.push(format!(
                "    disabled models: {}",
                settings.disabled_models.join(", ")
            ));
        }
        if !settings.fallback.is_empty() {
            lines.push(format!("    fallback: {}", settings.fallback.join(" -> ")));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::super::{model, provider, Cli, Command};
    use super::format_providers;
    use crate::config::AppConfig;
    use crate::infrastructure::openai::WireApi;
    use axum::{routing::get, Json, Router};
    use clap::Parser;
    use serde_json::json;
    use std::path::Path;

    /// Run `ano --config PATH ARGUMENTS...` and load the config it leaves.
    async fn ano(path: &Path, arguments: &[&str]) -> anyhow::Result<AppConfig> {
        let mut all = vec!["ano", "--config", path.to_str().unwrap()];
        all.extend(arguments);
        let config = AppConfig::load_or_default(path)?;
        match Cli::try_parse_from(all)?.command {
            Command::Provider(args) => provider::run(&config, path, args).await?,
            Command::Model(args) => model::run(&config, path, args).await?,
            _ => unreachable!(),
        }
        AppConfig::load(path)
    }

    async fn provider(path: &Path, command: &[&str]) -> anyhow::Result<AppConfig> {
        ano(path, &[&["provider"], command].concat()).await
    }

    async fn model(path: &Path, command: &[&str]) -> anyhow::Result<AppConfig> {
        ano(path, &[&["model"], command].concat()).await
    }

    #[tokio::test]
    async fn providers_and_their_models_are_managed_from_the_command_line() {
        let app = Router::new().route(
            "/v1/models",
            get(|| async { Json(json!({"data":[{"id":"qwen/qwen3"}, {"id":"qwen/qwen3-4b"}]})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");

        let config = provider(
            &path,
            &[
                "add",
                "lan",
                "--base-url",
                &url,
                "--model",
                "qwen/qwen3",
                "--fallback",
                "api",
            ],
        )
        .await
        .unwrap();
        assert_eq!(config.provider_model("lan"), Some("qwen/qwen3"));
        assert_eq!(config.providers["lan"].fallback, ["api"]);
        assert!(provider(&path, &["add", "lan"]).await.is_err());

        // Model names are checked against the provider's list.
        assert!(model(&path, &["disable", "missing", "--provider", "lan"])
            .await
            .is_err());
        let config = model(&path, &["disable", "qwen/qwen3-4b", "--provider", "lan"])
            .await
            .unwrap();
        let models = config.provider_settings("lan").unwrap().model_filter();
        assert!(!models.is_enabled("qwen/qwen3-4b"));
        assert!(models.is_enabled("qwen/qwen3"));
        let config = model(&path, &["enable", "qwen/qwen3-4b", "--provider", "lan"])
            .await
            .unwrap();
        assert!(config.providers["lan"].disabled_models.is_empty());
        let config = model(
            &path,
            &["disable", "other/*", "--provider", "lan", "--no-verify"],
        )
        .await
        .unwrap();
        assert_eq!(config.providers["lan"].disabled_models, ["other/*"]);
        // Without --provider, the models are those of [api].
        let config = model(&path, &["disable", "gpt-old*"]).await.unwrap();
        assert_eq!(config.api.disabled_models, ["gpt-old*"]);
        assert!(model(&path, &["disable"]).await.is_err());
        // A provider is enabled or disabled as a whole.
        assert!(provider(&path, &["disable", "lan", "qwen/qwen3"])
            .await
            .is_err());

        let config = provider(&path, &["disable", "lan"]).await.unwrap();
        assert!(!config.provider_enabled("lan"));
        assert!(config
            .select_model(&[crate::config::ModelRequest {
                provider: Some("lan".into()),
                ..Default::default()
            }])
            .is_err());
        let config = provider(&path, &["enable", "lan"]).await.unwrap();
        assert!(config.provider_enabled("lan"));
        assert!(provider(&path, &["disable", "api"]).await.is_err());

        let config = provider(
            &path,
            &["set", "lan", "--timeout-secs", "5", "--unset", "fallback"],
        )
        .await
        .unwrap();
        assert_eq!(config.provider_settings("lan").unwrap().timeout_secs, 5);
        assert!(config.providers["lan"].fallback.is_empty());
        let config = provider(&path, &["set", "lan", "--wire-api", "chat-completions"])
            .await
            .unwrap();
        assert_eq!(
            config.provider_settings("lan").unwrap().wire_api,
            WireApi::ChatCompletions
        );
        assert!(format_providers(&config).contains("(chat completions)"));
        let config = provider(&path, &["set", "lan", "--unset", "wire-api"])
            .await
            .unwrap();
        assert_eq!(
            config.provider_settings("lan").unwrap().wire_api,
            WireApi::Responses
        );
        assert!(provider(&path, &["set", "lan"]).await.is_err());
        assert!(
            provider(&path, &["set", "lan", "--model", "x", "--unset", "model"])
                .await
                .is_err()
        );
        let config = provider(&path, &["set", "api", "--model", "gpt-x"])
            .await
            .unwrap();
        assert_eq!(config.agent.settings.model, "gpt-x");

        let config = provider(&path, &["remove", "lan"]).await.unwrap();
        assert!(config.providers.is_empty());
        assert!(provider(&path, &["remove", "api"]).await.is_err());
    }

    #[tokio::test]
    async fn providers_without_a_model_list_use_the_registered_models() {
        // No /models route: the endpoint does not list its models.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, Router::new()).await.unwrap() });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        provider(&path, &["add", "sub", "--base-url", &url])
            .await
            .unwrap();

        // Listing succeeds even with nothing registered.
        model(&path, &["list", "--provider", "sub"]).await.unwrap();
        let error = model(&path, &["disable", "gpt-5.6-luna", "--provider", "sub"])
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("ano model add gpt-5.6-luna --provider sub"),
            "{error:#}"
        );

        let config = model(
            &path,
            &["add", "gpt-5.6-luna", "gpt-5.6-mini", "--provider", "sub"],
        )
        .await
        .unwrap();
        assert_eq!(
            config.providers["sub"].models,
            ["gpt-5.6-luna", "gpt-5.6-mini"]
        );
        let config = model(&path, &["disable", "gpt-5.6-mini", "--provider", "sub"])
            .await
            .unwrap();
        assert!(!config
            .provider_settings("sub")
            .unwrap()
            .model_filter()
            .is_enabled("gpt-5.6-mini"));
        model(&path, &["list", "--provider", "sub"]).await.unwrap();

        let config = model(
            &path,
            &[
                "remove",
                "gpt-5.6-luna",
                "gpt-5.6-mini",
                "--provider",
                "sub",
            ],
        )
        .await
        .unwrap();
        assert!(config.providers["sub"].models.is_empty());
        assert!(model(&path, &["add", "gpt-*", "--provider", "sub"])
            .await
            .is_err());
        // [api] registers models too.
        let config = model(&path, &["add", "gpt-5.6-luna"]).await.unwrap();
        assert_eq!(config.api.models, ["gpt-5.6-luna"]);
    }

    #[tokio::test]
    async fn the_default_provider_is_chosen_and_renamed_from_the_command_line() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            "[api]\nauth = 'chatgpt'\n[agent]\nmodel = 'gpt-5.6-luna'\n",
        )
        .unwrap();
        provider(
            &path,
            &[
                "add",
                "mainpc",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--model",
                "qwen3.8-27b",
            ],
        )
        .await
        .unwrap();

        let config = provider(&path, &["use", "mainpc"]).await.unwrap();
        assert_eq!(config.default_provider(), "mainpc");
        assert!(provider(&path, &["disable", "mainpc"]).await.is_err());
        // Models go to the default provider unless --provider says otherwise.
        let config = model(&path, &["add", "extra-model"]).await.unwrap();
        assert_eq!(config.providers["mainpc"].models, ["extra-model"]);
        let config = provider(&path, &["use", "api"]).await.unwrap();
        assert_eq!(config.default_provider(), "api");

        let config = provider(&path, &["rename", "api", "chatgpt"])
            .await
            .unwrap();
        assert_eq!(config.default_provider(), "chatgpt");
        assert_eq!(
            config.providers["chatgpt"].model.as_deref(),
            Some("gpt-5.6-luna")
        );
        assert_eq!(
            config.listed_provider_names().collect::<Vec<_>>(),
            ["chatgpt", "mainpc"]
        );
        let config = provider(&path, &["rename", "mainpc", "desktop"])
            .await
            .unwrap();
        assert_eq!(config.providers["desktop"].models, ["extra-model"]);
    }
}
