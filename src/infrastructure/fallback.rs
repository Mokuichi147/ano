//! Continue on the next configured provider when one cannot be reached or is
//! overloaded.

use super::openai::is_unavailable;
use crate::{
    application::ports::{DeltaSink, ResponseDelta, ResponsesApi},
    domain::provider::ModelFilter,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// How long a provider that failed is tried only after the others.
const COOLDOWN: Duration = Duration::from_secs(300);

/// One provider to fall back to.
pub struct FallbackTarget {
    pub name: String,
    pub client: Arc<dyn ResponsesApi>,
    /// The model to request there; `None` keeps the requested model.
    pub model: Option<String>,
    /// Models the provider may be used with, for a kept model.
    pub models: ModelFilter,
}

/// A client that sends each request to its primary provider, and to the
/// fallbacks in order when a provider is unavailable (unreachable, timed out,
/// or 408, 429, 5xx). Rejected requests are not retried elsewhere, and a
/// streamed answer is never restarted once text has been shown.
///
/// Conversations stay bound to the primary endpoint. A fallback is given the
/// history without encrypted reasoning, which only the provider that wrote
/// it can read, and its own reasoning is left out of the reply for the same
/// reason. A conversation compacted remotely by the primary cannot move.
pub struct FallbackClient {
    /// The primary first. Its `model` is always `None`.
    targets: Vec<FallbackTarget>,
    /// Per target: tried after the others until this time.
    unavailable_until: Mutex<Vec<Option<Instant>>>,
    /// The target that answered last, to report switches once.
    last_used: Mutex<usize>,
}

impl FallbackClient {
    pub fn new(name: &str, primary: Arc<dyn ResponsesApi>, fallbacks: Vec<FallbackTarget>) -> Self {
        let mut targets = vec![FallbackTarget {
            name: name.to_string(),
            client: primary,
            model: None,
            models: ModelFilter::default(),
        }];
        targets.extend(fallbacks);
        Self {
            unavailable_until: Mutex::new(vec![None; targets.len()]),
            targets,
            last_used: Mutex::new(0),
        }
    }

    /// Available targets in order, then the ones cooling down.
    fn order(&self) -> Vec<usize> {
        let now = Instant::now();
        let until = self
            .unavailable_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cooling = |index: &usize| until[*index].is_some_and(|until| until > now);
        let (waiting, ready): (Vec<usize>, Vec<usize>) = (0..self.targets.len()).partition(cooling);
        ready.into_iter().chain(waiting).collect()
    }

    /// The request as `index` receives it, or `None` when that provider
    /// cannot continue this conversation.
    fn request_for(&self, index: usize, payload: &Value) -> Option<Value> {
        if index == 0 {
            return Some(payload.clone());
        }
        let target = &self.targets[index];
        let items = payload["input"].as_array();
        if payload.get("previous_response_id").is_some()
            || items.is_some_and(|items| items.iter().any(|item| item["type"] == "compaction"))
        {
            return None;
        }
        let mut request = payload.clone();
        match &target.model {
            Some(model) => request["model"] = Value::String(model.clone()),
            None => {
                let model = payload["model"].as_str().unwrap_or_default();
                if !target.models.is_enabled(model) {
                    return None;
                }
            }
        }
        if let Some(items) = request["input"].as_array_mut() {
            items.retain(|item| item["type"] != "reasoning");
        }
        Some(request)
    }

    fn answered(&self, index: usize, mut response: Value) -> Value {
        self.unavailable_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())[index] = None;
        let mut last = self
            .last_used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *last != index {
            *last = index;
            eprintln!("(provider '{}' answered)", self.targets[index].name);
        }
        if index != 0 {
            if let Some(items) = response["output"].as_array_mut() {
                items.retain(|item| item["type"] != "reasoning");
            }
        }
        response
    }

    fn failed(&self, index: usize, error: &anyhow::Error) {
        self.unavailable_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())[index] =
            Some(Instant::now() + COOLDOWN);
        eprintln!(
            "warning: provider '{}' is unavailable: {error:#}",
            self.targets[index].name
        );
    }
}

#[async_trait]
impl ResponsesApi for FallbackClient {
    fn base_url(&self) -> &str {
        self.targets[0].client.base_url()
    }

    async fn create_response(&self, payload: &Value) -> Result<Value> {
        let mut last_error = None;
        for index in self.order() {
            let Some(request) = self.request_for(index, payload) else {
                continue;
            };
            match self.targets[index].client.create_response(&request).await {
                Ok(response) => return Ok(self.answered(index, response)),
                Err(error) if is_unavailable(&error) => {
                    self.failed(index, &error);
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("no provider can continue this conversation")))
    }

    async fn create_response_streaming(
        &self,
        payload: &Value,
        on_delta: DeltaSink<'_>,
    ) -> Result<Value> {
        let mut last_error = None;
        for index in self.order() {
            let Some(request) = self.request_for(index, payload) else {
                continue;
            };
            let shown = AtomicBool::new(false);
            let tracked = |delta: ResponseDelta<'_>| {
                shown.store(true, Ordering::Relaxed);
                on_delta(delta);
            };
            let result = self.targets[index]
                .client
                .create_response_streaming(&request, &tracked)
                .await;
            match result {
                Ok(response) => return Ok(self.answered(index, response)),
                Err(error) if is_unavailable(&error) && !shown.load(Ordering::Relaxed) => {
                    self.failed(index, &error);
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("no provider can continue this conversation")))
    }

    /// Only the primary can compact: its window is readable only there.
    async fn compact_response(&self, payload: &Value) -> Result<Value> {
        self.targets[0].client.compact_response(payload).await
    }

    fn supports_remote_compaction(&self) -> bool {
        self.targets[0].client.supports_remote_compaction()
    }

    /// Any provider may have to continue, so every request carries the whole
    /// history rather than a `previous_response_id`.
    fn requires_full_history(&self) -> bool {
        true
    }

    async fn list_models(&self) -> Result<Option<Vec<String>>> {
        self.targets[0].client.list_models().await
    }
}

#[cfg(test)]
mod tests {
    use super::{FallbackClient, FallbackTarget};
    use crate::{
        application::ports::{ResponseDelta, ResponsesApi},
        domain::provider::ModelFilter,
        infrastructure::openai::OpenAiClient,
    };
    use axum::{http::StatusCode, routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<Value>>>;

    /// A Responses endpoint that answers every request with `status`.
    async fn endpoint(status: StatusCode) -> (String, Seen) {
        let seen = Seen::default();
        let recorded = Arc::clone(&seen);
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(payload): Json<Value>| {
                let recorded = Arc::clone(&recorded);
                async move {
                    recorded.lock().unwrap().push(payload);
                    let body = if status.is_success() {
                        json!({"id":"r", "status":"completed", "output":[
                            {"type":"reasoning", "encrypted_content":"theirs"},
                            {"type":"message", "content":[{"type":"output_text", "text":"ok"}]}]})
                    } else {
                        json!({"error":{"message":"busy"}})
                    };
                    (status, Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, seen)
    }

    fn client(
        primary: &str,
        fallback: &str,
        model: Option<&str>,
        models: ModelFilter,
    ) -> FallbackClient {
        FallbackClient::new(
            "main",
            Arc::new(OpenAiClient::new("", primary)),
            vec![FallbackTarget {
                name: "backup".into(),
                client: Arc::new(OpenAiClient::new("", fallback)),
                model: model.map(str::to_string),
                models,
            }],
        )
    }

    fn payload() -> Value {
        json!({"model":"main-model", "input":[
            {"role":"user", "content":"hi"},
            {"type":"reasoning", "encrypted_content":"mine"}]})
    }

    #[tokio::test]
    async fn an_overloaded_provider_hands_over_and_is_skipped_for_a_while() {
        let (primary, primary_seen) = endpoint(StatusCode::SERVICE_UNAVAILABLE).await;
        let (fallback, fallback_seen) = endpoint(StatusCode::OK).await;
        let client = client(
            &primary,
            &fallback,
            Some("backup-model"),
            ModelFilter::default(),
        );
        assert_eq!(client.base_url(), primary.trim_end_matches('/'));

        let response = client.create_response(&payload()).await.unwrap();
        // Reasoning only the fallback can read stays out of the history.
        assert_eq!(response["output"].as_array().unwrap().len(), 1);
        let sent = fallback_seen.lock().unwrap()[0].clone();
        assert_eq!(sent["model"], "backup-model");
        assert_eq!(sent["input"], json!([{"role":"user", "content":"hi"}]));

        client.create_response(&payload()).await.unwrap();
        assert_eq!(primary_seen.lock().unwrap().len(), 1);
        assert_eq!(fallback_seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn rejected_requests_and_unreadable_histories_stay_on_the_primary() {
        let (rejecting, _) = endpoint(StatusCode::BAD_REQUEST).await;
        let (busy, _) = endpoint(StatusCode::SERVICE_UNAVAILABLE).await;
        let (fallback, fallback_seen) = endpoint(StatusCode::OK).await;
        assert!(client(&rejecting, &fallback, None, ModelFilter::default())
            .create_response(&payload())
            .await
            .is_err());

        let compacted =
            json!({"model":"m", "input":[{"type":"compaction", "encrypted_content":"x"}]});
        assert!(client(&busy, &fallback, None, ModelFilter::default())
            .create_response(&compacted)
            .await
            .is_err());

        // Without a model of its own, the fallback must allow the requested one.
        let disabled = ModelFilter {
            allowed: None,
            disabled: vec!["main-*".into()],
        };
        assert!(client(&busy, &fallback, None, disabled)
            .create_response(&payload())
            .await
            .is_err());
        assert!(fallback_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_provider_streams_from_the_fallback() {
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unreachable = format!("http://{}/v1", closed.local_addr().unwrap());
        drop(closed);
        let (fallback, fallback_seen) = endpoint(StatusCode::OK).await;
        let client = client(&unreachable, &fallback, None, ModelFilter::default());
        let text = Mutex::new(String::new());
        let sink = |delta: ResponseDelta<'_>| {
            if let ResponseDelta::Text(part) = delta {
                text.lock().unwrap().push_str(part);
            }
        };
        client
            .create_response_streaming(&payload(), &sink)
            .await
            .unwrap();
        assert_eq!(*text.lock().unwrap(), "ok");
        assert_eq!(fallback_seen.lock().unwrap()[0]["model"], "main-model");
    }
}
