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
    api_key: String,
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
        let api_key = match std::env::var(&settings.api_key_env) {
            Ok(value) => value,
            Err(_) if is_local_endpoint(&base_url) => {
                // LM Studio accepts an arbitrary bearer value when auth is not
                // enabled. This keeps a local setup free of a fake secret in
                // the shell while still sending an OpenAI-compatible header.
                "lm-studio".to_string()
            }
            Err(_) => bail!(
                "{} is not set; export it before using endpoint {}",
                settings.api_key_env,
                base_url
            ),
        };
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
            api_key: api_key.into(),
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
            let sent = self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(payload)
                .send()
                .await;
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

fn is_local_endpoint(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|address| address.is_loopback())
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
    use super::is_local_endpoint;

    #[test]
    fn detects_loopback_endpoints_by_host() {
        assert!(is_local_endpoint("http://127.0.0.1:1234/v1"));
        assert!(is_local_endpoint("http://localhost:1234/v1"));
        assert!(is_local_endpoint("http://[::1]:1234/v1"));
        assert!(!is_local_endpoint("https://api.openai.com/v1"));
        assert!(!is_local_endpoint("https://evil.test/?x=://localhost"));
        assert!(!is_local_endpoint("https://localhost.evil.test/v1"));
    }
}
