//! The settings page's API: the config file's settings, saved without
//! touching the rest of the file, and a restart that puts them into effect.
//!
//! The server reads the config once at startup, so a save changes nothing
//! until the restart; the page says so. API keys stay in the environment:
//! the page edits the names of the variables that hold them.

use super::{error_response, WebState};
use crate::config::{self, schema, SettingsConflict};
use axum::{extract::State, http::StatusCode, response::IntoResponse, response::Response, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{atomic::Ordering, Arc};

/// The schema of every setting, their defaults, and what the file sets.
pub(super) async fn read_settings(State(state): State<Arc<WebState>>) -> Response {
    let path = state.config_path.clone();
    match tokio::task::spawn_blocking(move || config::read_settings(&path)).await {
        Ok(Ok(file)) => Json(settings_json(&state, &file)).into_response(),
        Ok(Err(error)) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("{error:#}")),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "reading settings failed"),
    }
}

fn settings_json(state: &WebState, file: &config::SettingsFile) -> Value {
    json!({
        "path": state.config_path,
        "exists": file.exists,
        "revision": file.revision,
        "values": file.values,
        "schema": schema::SCHEMA,
        "defaults": schema::defaults(),
    })
}

#[derive(Deserialize)]
pub(super) struct SaveRequest {
    /// The `revision` the page read.
    revision: String,
    /// Every setting the file is to set, in the shape `values` came in.
    values: Value,
}

/// Write what the page sets. Refused when the file changed since the page
/// read it, or when the result would not be a valid config.
pub(super) async fn save_settings(
    State(state): State<Arc<WebState>>,
    Json(request): Json<SaveRequest>,
) -> Response {
    let path = state.config_path.clone();
    let saved = tokio::task::spawn_blocking(move || {
        config::save_settings(&path, &request.revision, &request.values)?;
        config::read_settings(&path)
    })
    .await;
    match saved {
        Ok(Ok(file)) => Json(settings_json(&state, &file)).into_response(),
        Ok(Err(error)) if error.downcast_ref::<SettingsConflict>().is_some() => error_response(
            StatusCode::CONFLICT,
            "設定ファイルがほかで変更されました。読み込み直してから変更してください",
        ),
        Ok(Err(error)) => error_response(StatusCode::BAD_REQUEST, &format!("{error:#}")),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "saving settings failed"),
    }
}

/// Stop the sessions and start the server again with the saved settings.
pub(super) async fn restart(State(state): State<Arc<WebState>>) -> Response {
    // Checked first, so a restart never leaves the server unable to start.
    let path = state.config_path.clone();
    let checked =
        tokio::task::spawn_blocking(move || config::AppConfig::load_or_default(&path)).await;
    match checked {
        // The new server is started for the same user, which must still be there.
        Ok(Ok(config)) if !config.has_user(&state.user) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("[users] に {} がありません", state.user),
            );
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("{error:#}"));
        }
        Err(_) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "restart failed"),
    }
    state.restarting.store(true, Ordering::SeqCst);
    state.restart.notify_one();
    StatusCode::ACCEPTED.into_response()
}
