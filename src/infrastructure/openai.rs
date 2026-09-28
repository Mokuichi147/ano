//! HTTP client for the OpenAI Responses API and compatible endpoints.

use crate::{
    application::ports::{replay_deltas, DeltaSink, ResponseDelta, ResponsesApi},
    domain::provider::ModelFilter,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::{Client, Response, StatusCode, Url};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiAuth {
    #[default]
    ApiKey,
    Chatgpt,
}

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
    pub auth: ApiAuth,
    /// ChatGPT の認証情報。省略時は OS のデータディレクトリの auth/chatgpt.json。
    pub chatgpt_auth_file: Option<PathBuf>,
    pub base_url: String,
    pub api_key_env: String,
    /// Total timeout for one Responses API request.
    pub timeout_secs: u64,
    /// Retries for connection failures and 429 / 5xx responses.
    pub max_retries: u32,
    /// Request server-sent events so answers appear while they are generated.
    /// An endpoint that ignores the request and returns JSON still works.
    pub stream: bool,
    /// `OPENAI_BASE_URL` で `base_url` を上書きするか。`[api]` だけが従い、
    /// `[providers.*]` の接続先は環境変数に左右されない。
    #[serde(skip, default = "yes")]
    pub use_base_url_env: bool,
    /// When set, only these models (exact names or `prefix*`) may be used.
    pub allowed_models: Option<Vec<String>>,
    pub disabled_models: Vec<String>,
    /// Providers to try in order when this one cannot be reached or is
    /// overloaded.
    pub fallback: Vec<String>,
}

fn yes() -> bool {
    true
}

impl Default for ApiSettings {
    fn default() -> Self {
        Self {
            auth: ApiAuth::ApiKey,
            chatgpt_auth_file: None,
            base_url: default_base_url(),
            api_key_env: default_api_key_env(),
            timeout_secs: 600,
            max_retries: 2,
            stream: true,
            use_base_url_env: true,
            allowed_models: None,
            disabled_models: Vec::new(),
            fallback: Vec::new(),
        }
    }
}

impl ApiSettings {
    /// 実際に接続する URL。`use_base_url_env` なら `OPENAI_BASE_URL` を優先する。
    pub fn effective_base_url(&self) -> String {
        std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|_| self.use_base_url_env)
            .unwrap_or_else(|| self.base_url.clone())
    }

    /// The models of this provider that may be used.
    pub fn models(&self) -> ModelFilter {
        ModelFilter {
            allowed: self.allowed_models.clone(),
            disabled: self.disabled_models.clone(),
        }
    }
}

/// `[providers.NAME]`：`[api]` とは別の名前付き接続先。省略した通信設定は
/// `[api]` から引き継ぎ、接続先そのもの（URL・認証）は引き継がない。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderSettings {
    /// `false` keeps the settings but refuses to use the provider.
    pub enabled: bool,
    pub auth: ApiAuth,
    pub chatgpt_auth_file: Option<PathBuf>,
    /// 省略時は OpenAI の公式 endpoint。
    pub base_url: Option<String>,
    /// 省略時は `OPENAI_API_KEY`。
    pub api_key_env: Option<String>,
    pub timeout_secs: Option<u64>,
    pub max_retries: Option<u32>,
    pub stream: Option<bool>,
    /// この接続先へ切り替えたときに使うモデル。省略時は切り替え前のモデルを使う。
    pub model: Option<String>,
    /// この接続先での `approval_mode = "auto"` の審査モデル。省略時は `model`。
    pub approval_model: Option<String>,
    pub allowed_models: Option<Vec<String>>,
    pub disabled_models: Vec<String>,
    pub fallback: Vec<String>,
}

impl Default for ProviderSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            auth: ApiAuth::default(),
            chatgpt_auth_file: None,
            base_url: None,
            api_key_env: None,
            timeout_secs: None,
            max_retries: None,
            stream: None,
            model: None,
            approval_model: None,
            allowed_models: None,
            disabled_models: Vec::new(),
            fallback: Vec::new(),
        }
    }
}

impl ProviderSettings {
    /// `[api]` の通信設定を引き継いだ、この接続先の設定。
    pub fn api_settings(&self, base: &ApiSettings) -> ApiSettings {
        ApiSettings {
            auth: self.auth,
            chatgpt_auth_file: self.chatgpt_auth_file.clone(),
            base_url: self.base_url.clone().unwrap_or_else(default_base_url),
            api_key_env: self.api_key_env.clone().unwrap_or_else(default_api_key_env),
            timeout_secs: self.timeout_secs.unwrap_or(base.timeout_secs),
            max_retries: self.max_retries.unwrap_or(base.max_retries),
            stream: self.stream.unwrap_or(base.stream),
            use_base_url_env: false,
            allowed_models: self.allowed_models.clone(),
            disabled_models: self.disabled_models.clone(),
            fallback: self.fallback.clone(),
        }
    }
}

/// The provider could not be reached, timed out, or is overloaded (408, 429,
/// or 5xx after the retries): another provider may take the request.
#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Unavailable {}

/// Whether `error` means the provider is unavailable rather than that the
/// request itself was rejected.
pub fn is_unavailable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Unavailable>().is_some()
}

/// Statuses that mean the provider is unavailable for now.
pub(super) fn is_unavailable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT || is_retryable(status) || status.is_server_error()
}

/// 認証方式に対応する通信アダプターを生成する。
pub fn create_client(settings: &ApiSettings) -> Result<Arc<dyn ResponsesApi>> {
    match settings.auth {
        ApiAuth::ApiKey => Ok(Arc::new(OpenAiClient::from_api_settings(settings)?)),
        ApiAuth::Chatgpt => Ok(Arc::new(super::chatgpt::ChatGptClient::from_settings(
            settings,
        )?)),
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
    stream: bool,
}

impl OpenAiClient {
    /// Build a client from `OPENAI_API_KEY` / `OPENAI_BASE_URL` and defaults.
    pub fn from_env() -> Result<Self> {
        Self::from_api_settings(&ApiSettings::default())
    }

    pub fn from_api_settings(settings: &ApiSettings) -> Result<Self> {
        if settings.auth != ApiAuth::ApiKey {
            bail!("ChatGPT 認証には infrastructure::openai::create_client を使用してください");
        }
        let base_url = settings.effective_base_url();
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
            stream: settings.stream,
        })
    }

    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            api_key: Some(api_key.into()).filter(|key| !key.is_empty()),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            max_retries: 0,
            stream: true,
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

    /// `POST /responses` with `stream: true`, reporting message text to
    /// `on_delta` as it arrives. A JSON reply (from an endpoint that does not
    /// stream) is accepted and its messages are reported at once.
    pub async fn create_response_streaming(
        &self,
        payload: &Value,
        on_delta: DeltaSink<'_>,
    ) -> Result<Value> {
        if !self.stream {
            let response = self.create_response(payload).await?;
            replay_deltas(&response, on_delta);
            return Ok(response);
        }
        let mut payload = payload.clone();
        payload["stream"] = Value::Bool(true);
        let response = self.send("responses", &payload).await?;
        let is_event_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        if !is_event_stream {
            let response = read_json(response, "responses").await?;
            replay_deltas(&response, on_delta);
            return Ok(response);
        }
        read_event_stream(response, on_delta).await
    }

    /// `GET /models`: the models the endpoint offers, sorted by name.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let mut request = self.http.get(format!("{}/models", self.base_url));
        if let Some(api_key) = &self.api_key {
            request = request.bearer_auth(api_key);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) if error.is_connect() || error.is_timeout() => {
                return Err(error).context(Unavailable("failed to list the models".into()))
            }
            Err(error) => return Err(error).context("failed to list the models"),
        };
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("listing the models failed ({status}): {}", truncate(&body));
        }
        let body = read_json(response, "models").await?;
        let mut models: Vec<String> = body["data"]
            .as_array()
            .context("the models response has no data array")?
            .iter()
            .filter_map(|model| model["id"].as_str().map(str::to_string))
            .collect();
        models.sort();
        models.dedup();
        Ok(models)
    }

    async fn post_json(&self, endpoint: &str, payload: &Value) -> Result<Value> {
        let response = self.send(endpoint, payload).await?;
        read_json(response, endpoint).await
    }

    /// Send a request, retrying connection failures and retryable statuses.
    /// Returns a successful response; an error status becomes an error.
    async fn send(&self, endpoint: &str, payload: &Value) -> Result<Response> {
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
                Err(error) if error.is_connect() || error.is_timeout() => {
                    return Err(error).context(Unavailable(format!(
                        "failed to send OpenAI {endpoint} request"
                    )))
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
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                let message = match serde_json::from_str::<Value>(&body) {
                    Ok(value) => error_text(&value),
                    Err(_) => truncate(&body),
                };
                let message = format!("OpenAI {endpoint} request failed ({status}): {message}");
                if is_unavailable_status(status) {
                    return Err(Unavailable(message).into());
                }
                bail!(message);
            }
            return Ok(response);
        }
    }
}

async fn read_json(response: Response, endpoint: &str) -> Result<Value> {
    let body = response
        .text()
        .await
        .with_context(|| format!("failed to read OpenAI {endpoint} response"))?;
    serde_json::from_str::<Value>(&body).with_context(|| {
        format!(
            "OpenAI returned invalid JSON for {endpoint}: {}",
            truncate(&body)
        )
    })
}

/// Read Responses API server-sent events until the terminal event and return
/// the response it carries.
pub(super) async fn read_event_stream(
    response: Response,
    on_delta: DeltaSink<'_>,
) -> Result<Value> {
    let mut events = response.bytes_stream().eventsource();
    // Finished output items by index, for endpoints whose terminal event
    // leaves the output out.
    let mut items = BTreeMap::new();
    let mut completed = None;
    while let Some(event) = events.next().await {
        let event = event
            .map_err(|error| anyhow::anyhow!("{error}"))
            .context("failed to read the streamed OpenAI responses reply")?;
        if event.data.trim() == "[DONE]" {
            break;
        }
        let data: Value = match serde_json::from_str(&event.data) {
            Ok(data) => data,
            // Comments and keep-alives carry no JSON.
            Err(_) => continue,
        };
        let kind = data["type"].as_str().unwrap_or(event.event.as_str());
        match kind {
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(delta) = data["delta"].as_str() {
                    on_delta(ResponseDelta::Text(delta));
                }
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                on_delta(ResponseDelta::Reasoning(
                    data["delta"].as_str().unwrap_or_default(),
                ));
            }
            "response.output_item.done" => {
                let item = &data["item"];
                if item["type"] == "message" {
                    on_delta(ResponseDelta::MessageDone);
                }
                let index = data["output_index"].as_u64().unwrap_or(items.len() as u64);
                items.insert(index, item.clone());
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                completed = Some(data["response"].clone());
                break;
            }
            "error" => {
                let message = data["message"]
                    .as_str()
                    .or_else(|| data["error"]["message"].as_str())
                    .unwrap_or("unknown error");
                bail!("OpenAI responses stream failed: {}", truncate(message));
            }
            _ => {}
        }
    }
    let mut response = completed
        .filter(Value::is_object)
        .context("the OpenAI responses stream ended before the response was complete")?;
    let has_output = response["output"]
        .as_array()
        .is_some_and(|output| !output.is_empty());
    if !has_output && !items.is_empty() {
        response["output"] = Value::Array(items.into_values().collect());
    }
    Ok(response)
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

    async fn create_response_streaming(
        &self,
        payload: &Value,
        on_delta: DeltaSink<'_>,
    ) -> Result<Value> {
        OpenAiClient::create_response_streaming(self, payload, on_delta).await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        OpenAiClient::list_models(self).await
    }

    fn supports_remote_compaction(&self) -> bool {
        is_openai_endpoint(&self.base_url)
    }
}

pub(super) fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || matches!(status.as_u16(), 500 | 502 | 503 | 504)
}

pub(super) fn backoff(attempt: u32, retry_after: Option<Duration>) -> Duration {
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

    /// Serve `body` from `/v1/responses` with the given content type and
    /// return the endpoint and the received payloads.
    async fn serve_once(
        content_type: &'static str,
        body: String,
    ) -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        let app = Router::new().route(
            "/v1/responses",
            post(move |Json(payload): Json<Value>| {
                let captured = Arc::clone(&captured);
                let body = body.clone();
                async move {
                    captured.lock().unwrap().push(payload);
                    ([("content-type", content_type)], body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (endpoint, seen, server)
    }

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect()
    }

    fn collect_deltas() -> (
        Arc<Mutex<Vec<String>>>,
        impl Fn(super::ResponseDelta<'_>) + Send + Sync,
    ) {
        let deltas = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&deltas);
        (deltas, move |delta| {
            sink.lock().unwrap().push(match delta {
                super::ResponseDelta::Text(text) => text.to_string(),
                super::ResponseDelta::MessageDone => "<done>".to_string(),
                super::ResponseDelta::Reasoning(_) => "<reasoning>".to_string(),
            })
        })
    }

    #[tokio::test]
    async fn streams_message_text_and_returns_the_completed_response() {
        let message = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"こんにちは、世界"}]});
        let body = sse(&[
            json!({"type":"response.created","response":{"id":"r1","status":"in_progress"}}),
            json!({"type":"response.reasoning_text.delta","output_index":0,"delta":"Think"}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}),
            json!({"type":"response.output_text.delta","output_index":0,"delta":"こんにちは"}),
            json!({"type":"response.output_text.delta","output_index":0,"delta":"、世界"}),
            json!({"type":"response.output_item.done","output_index":0,"item":message}),
            json!({"type":"response.completed","response":{"id":"r1","status":"completed","output":[message]}}),
        ]);
        let (endpoint, seen, server) = serve_once("text/event-stream", body).await;
        let (deltas, sink) = collect_deltas();
        let response = OpenAiClient::new("", &endpoint)
            .create_response_streaming(&json!({"model":"m"}), &sink)
            .await
            .unwrap();
        server.abort();
        assert_eq!(seen.lock().unwrap()[0]["stream"], true);
        assert_eq!(response["id"], "r1");
        assert_eq!(response["output"][0], message);
        assert_eq!(
            *deltas.lock().unwrap(),
            ["<reasoning>", "こんにちは", "、世界", "<done>"]
        );
    }

    #[tokio::test]
    async fn fills_in_output_from_finished_items_when_the_final_event_omits_it() {
        let call = json!({"type":"function_call","call_id":"c1","name":"echo","arguments":"{}"});
        let body = sse(&[
            json!({"type":"response.output_item.done","output_index":0,"item":call}),
            json!({"type":"response.completed","response":{"id":"r2","status":"completed"}}),
        ]);
        let (endpoint, _, server) = serve_once("text/event-stream", body).await;
        let (deltas, sink) = collect_deltas();
        let response = OpenAiClient::new("", &endpoint)
            .create_response_streaming(&json!({}), &sink)
            .await
            .unwrap();
        server.abort();
        assert_eq!(response["output"], json!([call]));
        assert!(deltas.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn accepts_a_json_reply_to_a_streaming_request() {
        let body = json!({"id":"r3","status":"completed","output":[
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"whole"}]}
        ]})
        .to_string();
        let (endpoint, _, server) = serve_once("application/json", body).await;
        let (deltas, sink) = collect_deltas();
        let response = OpenAiClient::new("", &endpoint)
            .create_response_streaming(&json!({}), &sink)
            .await
            .unwrap();
        server.abort();
        assert_eq!(response["id"], "r3");
        assert_eq!(*deltas.lock().unwrap(), ["whole", "<done>"]);
    }

    #[tokio::test]
    async fn stream_errors_and_truncated_streams_fail() {
        for (body, expected) in [
            (
                sse(&[json!({"type":"error","message":"overloaded"})]),
                "overloaded",
            ),
            (
                sse(&[json!({"type":"response.output_text.delta","delta":"par"})]),
                "ended before",
            ),
        ] {
            let (endpoint, _, server) = serve_once("text/event-stream", body).await;
            let (_, sink) = collect_deltas();
            let error = OpenAiClient::new("", &endpoint)
                .create_response_streaming(&json!({}), &sink)
                .await
                .unwrap_err();
            server.abort();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
    }

    #[tokio::test]
    async fn lists_the_models_of_the_endpoint() {
        let app = Router::new().route(
            "/v1/models",
            axum::routing::get(|headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer key");
                Json(
                    json!({"object":"list", "data":[{"id":"qwen"}, {"id":"gpt-5"}, {"id":"qwen"}]}),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = OpenAiClient::new(
            "key",
            format!("http://{}/v1", listener.local_addr().unwrap()),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert_eq!(client.list_models().await.unwrap(), ["gpt-5", "qwen"]);
    }
}
