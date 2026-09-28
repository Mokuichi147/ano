//! ChatGPT サブスクリプションで Codex の Responses 接続先を利用する。
//! 一般向け API とは制約が異なるため、API キー用アダプターとは分離する。

pub mod auth;

use super::openai::{
    backoff, is_retryable, is_unavailable_status, read_event_stream, ApiSettings, Unavailable,
};
use crate::application::ports::{replay_deltas, DeltaSink, ResponsesApi};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use auth::ChatGptAuth;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::time::Duration;

const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

#[derive(Clone)]
pub struct ChatGptClient {
    http: Client,
    auth: ChatGptAuth,
    base_url: String,
    max_retries: u32,
    display_stream: bool,
}

impl ChatGptClient {
    pub fn from_settings(settings: &ApiSettings) -> Result<Self> {
        // OAuth トークンを任意の互換 API へ送信させない。
        if settings.base_url.trim_end_matches('/') != "https://api.openai.com/v1"
            || settings.effective_base_url().trim_end_matches('/') != "https://api.openai.com/v1"
        {
            bail!("auth = \"chatgpt\" の接続先は固定です。api.base_url と OPENAI_BASE_URL のカスタム設定を外してください");
        }
        let auth = ChatGptAuth::new(settings.chatgpt_auth_file.as_deref())?;
        if !auth.status()?.logged_in {
            bail!("ChatGPT にログインしていません。ano auth login を実行してください");
        }
        Ok(Self {
            http: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(30))
                .timeout(Duration::from_secs(settings.timeout_secs.max(1)))
                .user_agent(concat!("ano/", env!("CARGO_PKG_VERSION")))
                .build()?,
            auth,
            base_url: CODEX_BASE_URL.into(),
            max_retries: settings.max_retries,
            display_stream: settings.stream,
        })
    }

    async fn response(&self, payload: &Value, sink: DeltaSink<'_>) -> Result<Value> {
        let payload = subscription_payload(payload)?;
        let mut credentials = self.auth.credentials(None).await?;
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            let result = self
                .http
                .post(format!("{}/responses", self.base_url))
                .bearer_auth(&credentials.access_token)
                .header("chatgpt-account-id", &credentials.account_id)
                .header("originator", "ano")
                .header("OpenAI-Beta", "responses=experimental")
                .header("Accept", "text/event-stream")
                .json(&payload)
                .send()
                .await;
            let response = match result {
                Ok(response) => response,
                Err(error) if error.is_connect() && attempt < self.max_retries => {
                    attempt += 1;
                    tokio::time::sleep(backoff(attempt, None)).await;
                    continue;
                }
                Err(error) if error.is_connect() || error.is_timeout() => {
                    return Err(error)
                        .context(Unavailable("ChatGPT にリクエストを送信できません".into()))
                }
                Err(error) => return Err(error).context("ChatGPT にリクエストを送信できません"),
            };
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                credentials = self
                    .auth
                    .credentials(Some(&credentials.access_token))
                    .await?;
                refreshed = true;
                continue;
            }
            if is_retryable(status) && attempt < self.max_retries {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_secs);
                attempt += 1;
                tokio::time::sleep(backoff(attempt, retry_after)).await;
                continue;
            }
            if !status.is_success() {
                if is_unavailable_status(status) {
                    let message = match status {
                        StatusCode::TOO_MANY_REQUESTS => "ChatGPT の利用上限またはレート制限に達しました。利用状況を確認し、時間をおいて再実行してください".to_string(),
                        _ => format!("ChatGPT が一時的に応答できません ({status})。時間をおいて再実行してください"),
                    };
                    return Err(Unavailable(message).into());
                }
                match status {
                    StatusCode::UNAUTHORIZED => bail!("ChatGPT 認証が失効しています。ano auth login を再実行してください"),
                    StatusCode::FORBIDDEN => bail!("ChatGPT へのアクセスが拒否されました。プラン、モデル、ワークスペースの利用権限を確認してください"),
                    _ => bail!("ChatGPT のリクエストに失敗しました ({status})。モデルと入力・ツールの対応状況を確認してください"),
                }
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            // 常に stream: true で送る。Codex の接続先は SSE を Content-Type なしで
            // 返すため、JSON と明示された応答以外は SSE として読む。
            if !content_type.starts_with("application/json") {
                // ストリーム開始後は再試行しない。重複表示・ツール再実行を防ぐ。
                return read_event_stream(response, sink).await;
            }
            let body = response
                .text()
                .await
                .context("ChatGPT の応答を読み取れません")?;
            let response: Value = serde_json::from_str(&body).with_context(|| {
                format!(
                    "ChatGPT の応答が不正です（HTTP {status}）: {:?}",
                    body.chars().take(300).collect::<String>()
                )
            })?;
            replay_deltas(&response, sink);
            return Ok(response);
        }
    }
}

#[async_trait]
impl ResponsesApi for ChatGptClient {
    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn requires_full_history(&self) -> bool {
        true
    }

    async fn create_response(&self, payload: &Value) -> Result<Value> {
        self.response(payload, &|_| {}).await
    }

    async fn create_response_streaming(
        &self,
        payload: &Value,
        sink: DeltaSink<'_>,
    ) -> Result<Value> {
        if self.display_stream {
            self.response(payload, sink).await
        } else {
            // 表示をまとめる場合も、サーバーとの通信には SSE が必要。
            let response = self.create_response(payload).await?;
            replay_deltas(&response, sink);
            Ok(response)
        }
    }

    async fn compact_response(&self, _payload: &Value) -> Result<Value> {
        bail!("ChatGPT 接続の会話圧縮には agent.compaction = \"auto\" または \"summary\" を使用してください")
    }
}

fn subscription_payload(payload: &Value) -> Result<Value> {
    let mut payload = payload
        .as_object()
        .context("Responses リクエストはオブジェクトである必要があります")?
        .clone();
    if payload
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null())
    {
        bail!("ChatGPT 接続では previous_response_id を利用できません。input に会話履歴全体を渡してください");
    }
    payload.remove("previous_response_id");
    for key in ["max_output_tokens", "temperature", "top_p"] {
        payload.remove(key);
    }
    payload.insert("store".into(), json!(false));
    payload.insert("stream".into(), json!(true));
    payload.entry("instructions").or_insert(json!(""));
    if let Some(Value::String(text)) = payload.get("input") {
        payload.insert("input".into(), json!([{"role": "user", "content": text}]));
    }
    let include = payload
        .entry("include")
        .or_insert(json!([]))
        .as_array_mut()
        .context("include は配列で指定してください")?;
    if !include
        .iter()
        .any(|value| value == "reasoning.encrypted_content")
    {
        include.push(json!("reasoning.encrypted_content"));
    }
    // サブスクリプションでは OpenAI 側が実行する MCP などは対象外。
    // ano 自身の function tool と、stdio / HTTP の直接 MCP 接続はそのまま利用できる。
    if payload
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(|tool| tool["type"] != "function"))
    {
        bail!("ChatGPT 接続は function tool に対応しています。MCP は transport = \"stdio\" または \"streamable_http\" で ano から直接接続してください");
    }
    if payload
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .filter_map(|item| item["content"].as_array())
                .flatten()
                .any(|part| part["type"] == "input_audio")
        })
    {
        bail!(
            "ChatGPT 接続では音声入力をサポートしていません。テキストまたは画像を使用してください"
        );
    }
    Ok(Value::Object(payload))
}

#[cfg(test)]
mod tests;
