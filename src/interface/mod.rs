//! Interface layer: the command line and the webhook server.
//!
//! `cli` is also the composition root of the `ano` binary: it builds the
//! infrastructure adapters and hands them to the application layer.

pub mod cli;
pub mod webhook;

use crate::{
    application::ports::ResponsesApi,
    config::AppConfig,
    infrastructure::{
        fallback::{FallbackClient, FallbackTarget},
        openai::create_client,
    },
};
use anyhow::Result;
use std::sync::Arc;

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
