//! Interface layer: the command line and the webhook server.
//!
//! `cli` is also the composition root of the `ano` binary: it builds the
//! infrastructure adapters and hands them to the application layer.

pub mod cli;
pub mod webhook;

use crate::{
    application::{
        agent::{ModelTarget, SubagentModels},
        ports::ResponsesApi,
    },
    config::{AppConfig, ModelRequest, ModelSelection},
    infrastructure::{
        fallback::{FallbackClient, FallbackTarget},
        openai::create_client,
    },
};
use anyhow::Result;
use std::{collections::HashMap, sync::Arc};

/// The client of the provider `name`. When the provider lists `fallback`
/// providers, requests move to them in order while it is unavailable.
/// Disabled fallbacks, and those whose own model is disabled, are left out;
/// one that cannot be set up (for example without a login) is skipped with a
/// warning so the primary still works.
pub(crate) fn connect_provider(config: &AppConfig, name: &str) -> Result<Arc<dyn ResponsesApi>> {
    let settings = config.provider_settings(name)?;
    let primary = create_client(&settings)?;
    let mut fallbacks = Vec::new();
    for next in &settings.fallback {
        if !config.provider_enabled(next) {
            continue;
        }
        let fallback = config.provider_settings(next)?;
        let model = config.provider_model(next).map(str::to_string);
        if model
            .as_ref()
            .is_some_and(|model| !fallback.model_filter().is_enabled(model))
        {
            continue;
        }
        match create_client(&fallback) {
            Ok(client) => fallbacks.push(FallbackTarget {
                name: next.clone(),
                client,
                model,
                models: fallback.model_filter(),
            }),
            Err(error) => {
                eprintln!("warning: skipped fallback provider '{next}' of '{name}': {error:#}")
            }
        }
    }
    Ok(if fallbacks.is_empty() {
        primary
    } else {
        Arc::new(FallbackClient::new(name, primary, fallbacks))
    })
}

/// Clients by provider name, so the main agent and its roles share the
/// client of a provider they have in common.
#[derive(Default, Clone)]
pub(crate) struct Connections {
    clients: HashMap<String, Arc<dyn ResponsesApi>>,
}

impl Connections {
    /// Use `client` for the provider `name`.
    pub(crate) fn insert(&mut self, name: &str, client: Arc<dyn ResponsesApi>) {
        self.clients.insert(name.to_string(), client);
    }

    /// The client of the provider `name`, connected on first use.
    pub(crate) fn get(&mut self, config: &AppConfig, name: &str) -> Result<Arc<dyn ResponsesApi>> {
        if let Some(client) = self.clients.get(name) {
            return Ok(Arc::clone(client));
        }
        let client = connect_provider(config, name)?;
        self.insert(name, Arc::clone(&client));
        Ok(client)
    }
}

/// The models of one run: the main agent's, and those of its roles.
pub(crate) struct RunModels {
    pub main: ModelTarget,
    /// The reviewer of `approval_mode = "auto"`.
    pub approval: ModelTarget,
    pub subagents: SubagentModels,
}

impl RunModels {
    /// Resolve the models of a run whose main agent uses `selection`. Roles
    /// in `[agent.roles]` apply their preset over it; `base` is what the
    /// `default` preset stands for (such as the run's environment).
    pub(crate) fn resolve(
        config: &AppConfig,
        base: &[ModelRequest],
        selection: &ModelSelection,
        connections: &mut Connections,
    ) -> Result<Self> {
        let main = ModelTarget {
            client: connections.get(config, &selection.choice.provider)?,
            model: selection.choice.model.clone(),
            reasoning_effort: selection.choice.reasoning_effort.clone(),
        };
        let mut role = |role: &str, preset: Option<&String>| -> Result<Option<ModelTarget>> {
            let Some(preset) = preset else {
                return Ok(None);
            };
            let chosen = config
                .select_role(base, &selection.choice, preset)
                .map_err(|error| error.context(format!("invalid preset of agent.roles.{role}")))?;
            let provider = &chosen.choice.provider;
            Ok(Some(ModelTarget {
                client: connections.get(config, provider).map_err(|error| {
                    error.context(format!(
                        "failed to connect provider '{provider}' of agent.roles.{role}"
                    ))
                })?,
                model: chosen.choice.model,
                reasoning_effort: chosen.choice.reasoning_effort,
            }))
        };
        let roles = &config.agent.roles;
        let subagents = SubagentModels {
            delegate: role("delegate", roles.delegate.as_ref())?,
            review: role("review", roles.review.as_ref())?,
        };
        // Without a preset, the reviewer is `approval_model` on the main
        // provider, with the model's default effort.
        let approval = match role("approval", roles.approval.as_ref())? {
            Some(target) => target,
            None => ModelTarget {
                client: Arc::clone(&main.client),
                model: selection
                    .approval_model
                    .clone()
                    .unwrap_or_else(|| selection.choice.model.clone()),
                reasoning_effort: None,
            },
        };
        Ok(Self {
            main,
            approval,
            subagents,
        })
    }
}
