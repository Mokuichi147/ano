//! The provider, model, and reasoning effort of a run: presets, the layers
//! that choose them, and their resolution.

use super::{validate_name, AppConfig, API_PROVIDER};
use crate::{
    application::settings::{validate_reasoning_effort, AgentSettings},
    domain::session::ModelChoice,
    infrastructure::openai::ApiSettings,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// The name of the preset that stands for the settings of `[agent]` (with
/// the preset of `[agent.roles].default`) and, in a run, of its environment.
pub const DEFAULT_PRESET: &str = "default";

/// A named set of a provider, a model, and a reasoning effort
/// (`[presets.NAME]`). Unset fields keep what lower layers chose.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PresetSettings {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// What the preset is for, shown in listings.
    pub description: Option<String>,
}

impl PresetSettings {
    /// The preset's settings in one line, such as `lan  model qwen  effort low`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(provider) = &self.provider {
            parts.push(format!("provider {provider}"));
        }
        if let Some(model) = &self.model {
            parts.push(format!("model {model}"));
        }
        if let Some(effort) = &self.reasoning_effort {
            parts.push(format!("effort {effort}"));
        }
        parts.join("  ")
    }
}

/// One layer of a provider, model, and reasoning effort choice, such as an
/// environment or the command line. Unset fields leave the lower layers in
/// effect; `preset` applies before the other fields of the same layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelRequest {
    pub preset: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

impl ModelRequest {
    pub fn preset(name: &str) -> Self {
        Self {
            preset: Some(name.to_string()),
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.preset.is_none()
            && self.provider.is_none()
            && self.model.is_none()
            && self.reasoning_effort.is_none()
    }

    /// Whether the layer chooses a model: a preset, a provider, or a model.
    /// A layer that only sets the effort keeps the model under it.
    pub fn chooses_model(&self) -> bool {
        self.preset.is_some() || self.provider.is_some() || self.model.is_some()
    }
}

impl From<&ModelChoice> for ModelRequest {
    fn from(choice: &ModelChoice) -> Self {
        Self {
            preset: None,
            provider: Some(choice.provider.clone()),
            model: Some(choice.model.clone()),
            reasoning_effort: choice.reasoning_effort.clone(),
        }
    }
}

/// The provider, model, and effort a run uses, with the provider's connection.
#[derive(Debug, Clone)]
pub struct ModelSelection {
    pub choice: ModelChoice,
    pub api: ApiSettings,
    /// Reviewer model for `approval_mode = "auto"`; `None` uses the model.
    pub approval_model: Option<String>,
}

impl ModelSelection {
    /// Apply the model and effort to `settings` of a run.
    pub fn apply_to(&self, settings: &mut AgentSettings) {
        settings.model.clone_from(&self.choice.model);
        settings
            .reasoning_effort
            .clone_from(&self.choice.reasoning_effort);
        settings.approval_model.clone_from(&self.approval_model);
    }

    /// `model on provider`, with the effort when one is set.
    pub fn describe(&self) -> String {
        let choice = &self.choice;
        match &choice.reasoning_effort {
            Some(effort) => format!(
                "model {} on {} (effort {effort})",
                choice.model, choice.provider
            ),
            None => format!("model {} on {}", choice.model, choice.provider),
        }
    }
}

impl AppConfig {
    pub(super) fn validate_preset(&self, name: &str, preset: &PresetSettings) -> Result<()> {
        if name == DEFAULT_PRESET {
            bail!("'{DEFAULT_PRESET}' is reserved for the settings of [agent]");
        }
        validate_name(name).context("invalid preset name")?;
        if preset.provider.is_none() && preset.model.is_none() && preset.reasoning_effort.is_none()
        {
            bail!("a preset must set provider, model, or reasoning_effort");
        }
        for (field, value) in [("provider", &preset.provider), ("model", &preset.model)] {
            if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
                bail!("{field} must not be empty");
            }
        }
        if let Some(provider) = &preset.provider {
            self.provider_settings(provider)?;
        }
        if let Some(effort) = &preset.reasoning_effort {
            validate_reasoning_effort("reasoning_effort", effort)?;
        }
        Ok(())
    }

    /// Fail unless `name` is a preset or `default`.
    pub fn check_preset(&self, name: &str) -> Result<()> {
        if name != DEFAULT_PRESET {
            self.preset(name)?;
        }
        Ok(())
    }

    /// The preset `name` from `[presets]`.
    pub fn preset(&self, name: &str) -> Result<&PresetSettings> {
        self.presets.get(name).with_context(|| {
            let names: Vec<&str> = std::iter::once(DEFAULT_PRESET)
                .chain(self.presets.keys().map(String::as_str))
                .collect();
            format!(
                "unknown preset '{name}'; choose one of: {}",
                names.join(", ")
            )
        })
    }

    /// The provider, model, and effort choice of the environment `name`.
    pub fn environment_request(&self, name: &str) -> Result<ModelRequest> {
        let environment = self.environment_for(name)?;
        Ok(ModelRequest {
            preset: environment.preset.clone(),
            provider: environment.provider.clone(),
            model: environment.model.clone(),
            reasoning_effort: environment.reasoning_effort.clone(),
        })
    }

    /// The provider runs use unless one is chosen: that of the preset in
    /// `[agent.roles].default`, `[agent].provider`, or `api` without either.
    pub fn default_provider(&self) -> &str {
        self.agent
            .roles
            .default
            .as_deref()
            .and_then(|name| self.presets.get(name))
            .and_then(|preset| preset.provider.as_deref())
            .or(self.agent.provider.as_deref())
            .unwrap_or(API_PROVIDER)
    }

    /// Resolve the provider, model, and reasoning effort of a run from
    /// `requests`, lowest precedence first (for example the environment, then
    /// the command line).
    ///
    /// The settings of `[agent]` come first, then the preset of
    /// `[agent.roles].default`. In each request, its preset applies first and its
    /// own fields over it. A request that names a model uses it. A request
    /// that names only a provider switches to that provider's model
    /// (`[agent].model` for `api`) when it has one, and otherwise keeps the
    /// model from lower layers. The effort is kept until a layer sets one.
    pub fn select_model(&self, requests: &[ModelRequest]) -> Result<ModelSelection> {
        let base = ModelRequest {
            provider: Some(
                self.agent
                    .provider
                    .clone()
                    .unwrap_or_else(|| API_PROVIDER.into()),
            ),
            reasoning_effort: self.agent.reasoning_effort.clone(),
            ..ModelRequest::default()
        };
        let default = ModelRequest::preset(
            self.agent
                .roles
                .default
                .as_deref()
                .unwrap_or(DEFAULT_PRESET),
        );
        let mut provider = API_PROVIDER.to_string();
        let mut model = self.agent.model.clone();
        let mut reasoning_effort = None;
        let mut approval_model = self.agent.approval_model.clone();
        for request in [&base, &default].into_iter().chain(requests) {
            let preset = match request.preset.as_deref() {
                None | Some(DEFAULT_PRESET) => None,
                Some(name) => Some(self.preset(name)?),
            };
            let layers = preset
                .map(|preset| (&preset.provider, &preset.model, &preset.reasoning_effort))
                .into_iter()
                .chain([(&request.provider, &request.model, &request.reasoning_effort)]);
            for (layer_provider, layer_model, layer_effort) in layers {
                if let Some(name) = layer_provider {
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
                if let Some(name) = layer_model {
                    if name.trim().is_empty() {
                        bail!("model must not be empty");
                    }
                    model.clone_from(name);
                }
                if let Some(effort) = layer_effort {
                    validate_reasoning_effort("reasoning effort", effort)?;
                    reasoning_effort = Some(effort.clone());
                }
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
            choice: ModelChoice {
                provider,
                model,
                reasoning_effort,
            },
            approval_model,
        })
    }

    /// The selection of a role that uses `preset`, applied over `current`,
    /// the main agent's selection. `default` is the selection of `base`,
    /// the layers under any choice made during the run (such as the
    /// environment).
    pub fn select_role(
        &self,
        base: &[ModelRequest],
        current: &ModelChoice,
        preset: &str,
    ) -> Result<ModelSelection> {
        if preset == DEFAULT_PRESET {
            return self.select_model(base);
        }
        self.select_model(&[ModelRequest::from(current), ModelRequest::preset(preset)])
    }
}
