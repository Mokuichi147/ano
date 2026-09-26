//! HTTP client for the OpenAI Responses API and compatible endpoints.

use crate::application::ports::ResponsesApi;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_ERROR_BODY_CHARS: usize = 2000;

fn default_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_api_key_env() -> String {
    "OPENAI_API_KEY".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiSettings {
    pub base_url: String,
    pub api_key_env: String,
    /// Total timeout for one Responses API request.
    pub timeout_secs: u64,
    /// Retries for connection failures and 429 / 5xx responses.
    pub max_retries: u32,
}

impl Default for ApiSettings {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            api_key_env: default_api_key_env(),
            timeout_secs: 600,
            max_retries: 2,
        }
    }
}

#[derive(Clone)]
pub struct OpenAiClient {
    http: Client,
    /// Sent as a bearer token when present. Local and self-hosted servers
    /// usually run without authentication.
    api_key: Option<String>,
    base_url: String,
    max_retries: u32,
}

impl OpenAiClient {
    /// Build a client from `OPENAI_API_KEY` / `OPENAI_BASE_URL` and defaults.
    pub fn from_env() -> Result<Self> {
        Self::from_api_settings(&ApiSettings::default())
    }

    pub fn from_api_settings(settings: &ApiSettings) -> Result<Self> {
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| settings.base_url.clone());
        let api_key = resolve_api_key(
            &base_url,
            &settings.api_key_env,
            std::env::var(&settings.api_key_env).ok(),
        )?;
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(Duration::from_secs(settings.timeout_secs.max(1)))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            max_retries: settings.max_retries,
        })
    }

    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            api_key: Some(api_key.into()).filter(|key| !key.is_empty()),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            max_retries: 0,
        }
    }

    pub async fn create_response(&self, payload: &Value) -> Result<Value> {
        self.post_json("responses", payload).await
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn compact_response(&self, payload: &Value) -> Result<Value> {
        self.post_json("responses/compact", payload).await
    }

    async fn post_json(&self, endpoint: &str, payload: &Value) -> Result<Value> {
        let url = format!("{}/{}", self.base_url, endpoint);
        let mut attempt = 0;
        loop {
            let mut request = self.http.post(&url).json(payload);
            if let Some(api_key) = &self.api_key {
                request = request.bearer_auth(api_key);
            }
            let sent = request.send().await;
            let response = match sent {
                Ok(response) => response,
                Err(error) if error.is_connect() && attempt < self.max_retries => {
                    attempt += 1;
                    tokio::time::sleep(backoff(attempt, None)).await;
                    continue;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to send OpenAI {endpoint} request"))
                }
            };

            let status = response.status();
            if is_retryable(status) && attempt < self.max_retries {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                attempt += 1;
                tokio::time::sleep(backoff(attempt, retry_after)).await;
                continue;
            }

            let body = response
                .text()
                .await
                .with_context(|| format!("failed to read OpenAI {endpoint} response"))?;
            let parsed = serde_json::from_str::<Value>(&body);
            if !status.is_success() {
                let message = match &parsed {
                    Ok(value) => error_text(value),
                    Err(_) => truncate(&body),
                };
                bail!("OpenAI {endpoint} request failed ({status}): {message}");
            }
            return parsed.with_context(|| {
                format!(
                    "OpenAI returned invalid JSON for {endpoint}: {}",
                    truncate(&body)
                )
            });
        }
    }
}

#[async_trait]
impl ResponsesApi for OpenAiClient {
    fn base_url(&self) -> &str {
        OpenAiClient::base_url(self)
    }

    async fn create_response(&self, payload: &Value) -> Result<Value> {
        OpenAiClient::create_response(self, payload).await
    }

    async fn compact_response(&self, payload: &Value) -> Result<Value> {
        OpenAiClient::compact_response(self, payload).await
    }
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || matches!(status.as_u16(), 500 | 502 | 503 | 504)
}

fn backoff(attempt: u32, retry_after: Option<Duration>) -> Duration {
    retry_after
        .unwrap_or_else(|| Duration::from_millis(500 * 2_u64.pow(attempt.min(6))))
        .min(MAX_RETRY_DELAY)
}

/// The API key to send. Only the official OpenAI endpoint requires one;
/// other endpoints (LM Studio, Ollama, vLLM, llama.cpp, and so on, whether on
/// this machine or elsewhere on the network) are called without an
/// `Authorization` header when no key is configured. A server that does
/// require a key reports its own authentication error.
fn resolve_api_key(base_url: &str, key_env: &str, value: Option<String>) -> Result<Option<String>> {
    match value.filter(|value| !value.trim().is_empty()) {
        Some(value) => Ok(Some(value)),
        None if is_openai_endpoint(base_url) => bail!(
            "{key_env} is not set; it is required for {base_url}. To use a local or self-hosted model instead, set base_url in the [api] section of config.toml (for example http://127.0.0.1:1234/v1) or the OPENAI_BASE_URL environment variable; config.toml is read from the current directory unless --config is given"
        ),
        None => Ok(None),
    }
}

fn is_openai_endpoint(base_url: &str) -> bool {
    Url::parse(base_url)
        .ok()
        .and_then(|url| {
            url.host_str()
                .map(|host| host.eq_ignore_ascii_case("api.openai.com"))
        })
        .unwrap_or(false)
}

fn error_text(value: &Value) -> String {
    value["error"]["message"]
        .as_str()
        .or_else(|| value["message"].as_str())
        .map(truncate)
        .unwrap_or_else(|| "unknown error".to_string())
}

fn truncate(text: &str) -> String {
    let mut result = text.chars().take(MAX_ERROR_BODY_CHARS).collect::<String>();
    if text.chars().count() > MAX_ERROR_BODY_CHARS {
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{resolve_api_key, OpenAiClient};
    use axum::{http::HeaderMap, routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    #[test]
    fn only_the_openai_endpoint_requires_a_key() {
        let error = resolve_api_key("https://api.openai.com/v1", "OPENAI_API_KEY", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("OPENAI_API_KEY is not set"));
        assert!(error.contains("base_url"));
        assert!(resolve_api_key(
            "https://API.OPENAI.COM/v1",
            "OPENAI_API_KEY",
            Some(" ".into())
        )
        .is_err());
        for endpoint in [
            "http://127.0.0.1:1234/v1",
            "http://localhost:11434/v1",
            "http://192.168.1.20:8000/v1",
            "http://ollama.local:11434/v1",
            "http://host.docker.internal:1234/v1",
            "https://api.openai.com.example.test/v1",
        ] {
            assert_eq!(
                resolve_api_key(endpoint, "OPENAI_API_KEY", None).unwrap(),
                None,
                "{endpoint}"
            );
        }
        assert_eq!(
            resolve_api_key("http://192.168.1.20:8000/v1", "K", Some("secret".into())).unwrap(),
            Some("secret".into())
        );
    }

    #[tokio::test]
    async fn sends_authorization_only_when_a_key_is_configured() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let app = Router::new().route(
            "/v1/responses",
            post(move |headers: HeaderMap| {
                let captured = Arc::clone(&captured);
                async move {
                    captured.lock().unwrap().push(
                        headers
                            .get("authorization")
                            .map(|value| value.to_str().unwrap().to_string()),
                    );
                    Json(json!({"id": "r", "status": "completed", "output": []}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let payload: Value = json!({});
        for key in ["", "secret"] {
            OpenAiClient::new(key, &endpoint)
                .create_response(&payload)
                .await
                .unwrap();
        }
        server.abort();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![None, Some("Bearer secret".to_string())]
        );
    }
}
