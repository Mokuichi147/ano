//! Responses API を持たず `/chat/completions` だけを受け付けるサーバーへ接続する。
//! 要求を Chat Completions 形式へ、応答を Responses 形式へ変換するため、
//! エージェントと会話履歴は接続先の形式を意識しない。
//!
//! サーバーは応答を保存しないので、毎回履歴全体を送る。推論の本文は
//! `reasoning_text` の項目として履歴に残し、次の assistant メッセージの
//! `reasoning_content` として返す（llama.cpp や vLLM はこれを読む）。

use super::openai::{read_json, truncate, OpenAiClient};
use crate::application::ports::{replay_deltas, DeltaSink, ResponseDelta, ResponsesApi};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::Response;
use serde_json::{json, Map, Value};

const ENDPOINT: &str = "chat/completions";

#[derive(Clone)]
pub struct ChatCompletionsClient {
    /// 送信・再試行・モデル一覧は Responses 用のクライアントと共通。
    inner: OpenAiClient,
}

impl ChatCompletionsClient {
    pub fn new(inner: OpenAiClient) -> Self {
        Self { inner }
    }

    async fn complete(&self, payload: &Value) -> Result<Value> {
        let request = chat_request(payload, false)?;
        let response = self.inner.send(ENDPOINT, &request).await?;
        response_from_chat(&read_json(response, ENDPOINT).await?)
    }

    async fn complete_streaming(&self, payload: &Value, on_delta: DeltaSink<'_>) -> Result<Value> {
        let request = chat_request(payload, true)?;
        let response = self.inner.send(ENDPOINT, &request).await?;
        let is_event_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        if !is_event_stream {
            let response = response_from_chat(&read_json(response, ENDPOINT).await?)?;
            replay(&response, on_delta);
            return Ok(response);
        }
        read_chat_stream(response, on_delta).await
    }
}

#[async_trait]
impl ResponsesApi for ChatCompletionsClient {
    fn base_url(&self) -> &str {
        self.inner.base_url()
    }

    async fn create_response(&self, payload: &Value) -> Result<Value> {
        self.complete(payload).await
    }

    async fn create_response_streaming(
        &self,
        payload: &Value,
        on_delta: DeltaSink<'_>,
    ) -> Result<Value> {
        if !self.inner.streams() {
            let response = self.complete(payload).await?;
            replay(&response, on_delta);
            return Ok(response);
        }
        self.complete_streaming(payload, on_delta).await
    }

    async fn compact_response(&self, _payload: &Value) -> Result<Value> {
        bail!("Chat Completions の接続先は /responses/compact に対応していません。agent.compaction = \"auto\" または \"summary\" を使用してください")
    }

    fn requires_full_history(&self) -> bool {
        true
    }

    async fn list_models(&self) -> Result<Option<Vec<String>>> {
        self.inner.list_models().await
    }

    async fn context_window(&self, model: &str) -> Option<u64> {
        self.inner.loaded_context_length(model).await
    }
}

/// Responses 形式の要求を Chat Completions 形式へ変換する。
fn chat_request(payload: &Value, stream: bool) -> Result<Value> {
    let payload = payload
        .as_object()
        .context("Responses リクエストはオブジェクトである必要があります")?;
    if payload
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null())
    {
        bail!("Chat Completions の接続先では previous_response_id を利用できません。input に会話履歴全体を渡してください");
    }
    let mut messages = Vec::new();
    if let Some(instructions) = payload
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
    {
        messages.push(json!({"role": "system", "content": instructions}));
    }
    match payload.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => append_items(&mut messages, items)?,
        None | Some(Value::Null) => {}
        Some(_) => bail!("input は文字列または配列で指定してください"),
    }

    let mut request = Map::new();
    request.insert(
        "model".into(),
        payload.get("model").cloned().unwrap_or(Value::Null),
    );
    request.insert("messages".into(), Value::Array(messages));
    let tools = payload
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(chat_tool)
        .collect::<Result<Vec<_>>>()?;
    let tool_choice = payload.get("tool_choice").filter(|value| !value.is_null());
    // tool を使わせない回は定義ごと外す。tool_choice を無視するサーバーでも
    // 呼び出しが返らないようにするため。
    if !tools.is_empty() && tool_choice.and_then(Value::as_str) != Some("none") {
        request.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = tool_choice {
            request.insert("tool_choice".into(), chat_tool_choice(choice));
        }
        if let Some(parallel) = payload
            .get("parallel_tool_calls")
            .filter(|value| value.is_boolean())
        {
            request.insert("parallel_tool_calls".into(), parallel.clone());
        }
    }
    if let Some(limit) = payload
        .get("max_output_tokens")
        .filter(|value| !value.is_null())
    {
        request.insert("max_tokens".into(), limit.clone());
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = payload.get(key).filter(|value| !value.is_null()) {
            request.insert(key.into(), value.clone());
        }
    }
    if let Some(effort) = payload
        .get("reasoning")
        .and_then(|reasoning| reasoning["effort"].as_str())
    {
        request.insert("reasoning_effort".into(), json!(effort));
    }
    if let Some(format) = payload
        .get("text")
        .and_then(|text| text.get("format"))
        .and_then(response_format)
    {
        request.insert("response_format".into(), format);
    }
    if stream {
        request.insert("stream".into(), json!(true));
        request.insert("stream_options".into(), json!({"include_usage": true}));
    }
    Ok(Value::Object(request))
}

/// 履歴の項目を Chat Completions のメッセージにする。1 つの応答の推論・本文・
/// tool 呼び出しは 1 つの assistant メッセージにまとめる。
fn append_items(messages: &mut Vec<Value>, items: &[Value]) -> Result<()> {
    // 直前に読んだ推論。次の assistant メッセージに添える。
    let mut reasoning: Option<String> = None;
    let assistant = |content: Value, reasoning: Option<String>| {
        let mut message = json!({"role": "assistant", "content": content});
        if let Some(reasoning) = reasoning {
            message["reasoning_content"] = json!(reasoning);
        }
        message
    };
    for item in items {
        match item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
        {
            "message" => match item["role"].as_str().unwrap_or("user") {
                "assistant" => {
                    let text = text_of(&item["content"]);
                    messages.push(assistant(json!(text), reasoning.take()));
                }
                "user" => messages.push(json!({
                    "role": "user",
                    "content": user_content(&item["content"])?,
                })),
                "system" | "developer" => messages.push(json!({
                    "role": "system",
                    "content": text_of(&item["content"]),
                })),
                role => bail!("Chat Completions の接続先へ送れないメッセージの role です: {role}"),
            },
            "function_call" => {
                let arguments = match &item["arguments"] {
                    Value::String(text) => text.clone(),
                    Value::Null => "{}".into(),
                    value => value.to_string(),
                };
                let call = json!({
                    "id": item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {"name": item["name"], "arguments": arguments},
                });
                match messages.last_mut() {
                    // 同じ応答の本文に続く呼び出し。
                    Some(last) if last["role"] == "assistant" && reasoning.is_none() => {
                        if last["content"].as_str().is_some_and(str::is_empty) {
                            last["content"] = Value::Null;
                        }
                        match last["tool_calls"].as_array_mut() {
                            Some(calls) => calls.push(call),
                            None => last["tool_calls"] = json!([call]),
                        }
                    }
                    _ => {
                        let mut message = assistant(Value::Null, reasoning.take());
                        message["tool_calls"] = json!([call]);
                        messages.push(message);
                    }
                }
            }
            "function_call_output" => messages.push(json!({
                "role": "tool",
                "tool_call_id": item["call_id"],
                "content": match &item["output"] {
                    Value::String(text) => text.clone(),
                    output => text_of(output),
                },
            })),
            "reasoning" => {
                let text = reasoning_text(item);
                if !text.trim().is_empty() {
                    match &mut reasoning {
                        Some(previous) => {
                            previous.push_str("\n\n");
                            previous.push_str(&text);
                        }
                        None => reasoning = Some(text),
                    }
                }
            }
            "input_audio" => messages.push(json!({
                "role": "user",
                "content": [{"type": "input_audio", "input_audio": item["input_audio"]}],
            })),
            kind => bail!("Chat Completions の接続先へ送れない履歴の項目です: {kind}"),
        }
    }
    Ok(())
}

/// 文字列か、テキストの部品の配列から本文を取り出す。
fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str().or_else(|| part["refusal"].as_str()))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// user メッセージの内容。テキストだけなら文字列にまとめる（配列を受け付けない
/// チャットテンプレートがあるため）。
fn user_content(content: &Value) -> Result<Value> {
    let Value::Array(parts) = content else {
        return Ok(json!(text_of(content)));
    };
    let parts = parts
        .iter()
        .map(|part| {
            Ok(match part["type"].as_str().unwrap_or_default() {
                "input_text" | "output_text" | "text" => {
                    json!({"type": "text", "text": part["text"]})
                }
                "input_image" => {
                    let url = part["image_url"].as_str().context(
                        "Chat Completions の接続先では URL かデータ URL の画像だけを送れます",
                    )?;
                    let mut image = json!({"url": url});
                    if let Some(detail) = part["detail"].as_str() {
                        image["detail"] = json!(detail);
                    }
                    json!({"type": "image_url", "image_url": image})
                }
                "input_audio" => {
                    json!({"type": "input_audio", "input_audio": part["input_audio"]})
                }
                kind => bail!("Chat Completions の接続先へ送れない入力の種類です: {kind}"),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if parts.iter().all(|part| part["type"] == "text") {
        return Ok(json!(text_of(&Value::Array(parts))));
    }
    Ok(Value::Array(parts))
}

/// 推論の本文。なければ要約。暗号化された推論は他の接続先では読めない。
fn reasoning_text(item: &Value) -> String {
    let parts = |key: &str, kind: &str| {
        item[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|part| part["type"] == kind)
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let content = parts("content", "reasoning_text");
    if content.trim().is_empty() {
        parts("summary", "summary_text")
    } else {
        content
    }
}

fn chat_tool(tool: &Value) -> Result<Value> {
    if tool["type"] != "function" {
        bail!(
            "Chat Completions の接続先は function tool だけに対応しています（{} は使えません）。MCP は transport = \"stdio\" または \"streamable_http\" で ano から直接接続してください",
            tool["type"].as_str().unwrap_or("種類なし")
        );
    }
    let mut function = Map::new();
    for key in ["name", "description", "parameters", "strict"] {
        if let Some(value) = tool.get(key).filter(|value| !value.is_null()) {
            function.insert(key.into(), value.clone());
        }
    }
    Ok(json!({"type": "function", "function": function}))
}

fn chat_tool_choice(choice: &Value) -> Value {
    match choice["type"].as_str() {
        Some("function") => json!({"type": "function", "function": {"name": choice["name"]}}),
        _ => choice.clone(),
    }
}

/// `text.format` を `response_format` にする。`text` 形式は既定なので送らない。
fn response_format(format: &Value) -> Option<Value> {
    match format["type"].as_str()? {
        "json_schema" => {
            let mut schema = Map::new();
            for key in ["name", "description", "schema", "strict"] {
                if let Some(value) = format.get(key).filter(|value| !value.is_null()) {
                    schema.insert(key.into(), value.clone());
                }
            }
            Some(json!({"type": "json_schema", "json_schema": schema}))
        }
        "json_object" => Some(json!({"type": "json_object"})),
        _ => None,
    }
}

/// 1 つの応答の内容。ストリームでは断片を順に積み上げる。
#[derive(Debug, Default)]
struct Reply {
    id: Option<String>,
    model: Option<String>,
    reasoning: String,
    text: String,
    refusal: String,
    calls: Vec<Call>,
    finish_reason: Option<String>,
    usage: Option<Value>,
}

#[derive(Debug, Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}

impl Reply {
    fn into_response(self) -> Value {
        let id = self
            .id
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()));
        let mut output = Vec::new();
        if !self.reasoning.trim().is_empty() {
            output.push(json!({
                "type": "reasoning",
                "id": format!("rs_{id}"),
                "summary": [],
                "content": [{"type": "reasoning_text", "text": self.reasoning}],
            }));
        }
        let mut content = Vec::new();
        if !self.text.trim().is_empty() {
            content.push(json!({"type": "output_text", "text": self.text, "annotations": []}));
        }
        if !self.refusal.trim().is_empty() {
            content.push(json!({"type": "refusal", "refusal": self.refusal}));
        }
        if !content.is_empty() {
            output.push(json!({
                "type": "message",
                "id": format!("msg_{id}"),
                "role": "assistant",
                "status": "completed",
                "content": content,
            }));
        }
        for call in self.calls.into_iter().filter(|call| !call.name.is_empty()) {
            let call_id = if call.id.is_empty() {
                format!("call_{}", uuid::Uuid::new_v4().simple())
            } else {
                call.id
            };
            output.push(json!({
                "type": "function_call",
                "id": format!("fc_{call_id}"),
                "call_id": call_id,
                "name": call.name,
                "arguments": if call.arguments.trim().is_empty() { "{}".into() } else { call.arguments },
                "status": "completed",
            }));
        }
        let incomplete = match self.finish_reason.as_deref() {
            Some("length") => Some("max_output_tokens"),
            Some("content_filter") => Some("content_filter"),
            _ => None,
        };
        let mut response = json!({
            "id": id,
            "object": "response",
            "model": self.model,
            "status": if incomplete.is_some() { "incomplete" } else { "completed" },
            "output": output,
        });
        if let Some(reason) = incomplete {
            response["incomplete_details"] = json!({"reason": reason});
        }
        if let Some(usage) = self.usage.as_ref().and_then(usage_from_chat) {
            response["usage"] = usage;
        }
        response
    }
}

/// Chat Completions の使用量を Responses 形式にする。
fn usage_from_chat(usage: &Value) -> Option<Value> {
    let input = usage["prompt_tokens"].as_u64()?;
    let output = usage["completion_tokens"].as_u64()?;
    Some(json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": input.saturating_add(output),
        "input_tokens_details": {
            "cached_tokens": usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
        },
        "output_tokens_details": {
            "reasoning_tokens": usage["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or(0),
        },
    }))
}

fn error_message(error: &Value) -> String {
    error["message"]
        .as_str()
        .or_else(|| error.as_str())
        .map(truncate)
        .unwrap_or_else(|| truncate(&error.to_string()))
}

/// ストリームしなかった応答の推論と本文を `on_delta` に渡す。
fn replay(response: &Value, on_delta: DeltaSink<'_>) {
    for item in response["output"].as_array().into_iter().flatten() {
        let text = reasoning_text(item);
        if item["type"] == "reasoning" && !text.trim().is_empty() {
            on_delta(ResponseDelta::Reasoning(&text));
        }
    }
    replay_deltas(response, on_delta);
}

/// 推論の本文。サーバーにより `reasoning_content` か `reasoning` で返る。
fn reasoning_of(value: &Value) -> Option<&str> {
    value["reasoning_content"]
        .as_str()
        .or_else(|| value["reasoning"].as_str())
        .filter(|text| !text.is_empty())
}

/// ストリームしない応答を Responses 形式にする。
fn response_from_chat(body: &Value) -> Result<Value> {
    if let Some(error) = body.get("error").filter(|error| !error.is_null()) {
        bail!("OpenAI {ENDPOINT} request failed: {}", error_message(error));
    }
    let choice = body["choices"]
        .as_array()
        .and_then(|choices| choices.first())
        .with_context(|| {
            format!(
                "the {ENDPOINT} response has no choices: {}",
                truncate(&body.to_string())
            )
        })?;
    let message = &choice["message"];
    let calls = message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|call| Call {
            id: call["id"].as_str().unwrap_or_default().into(),
            name: call["function"]["name"].as_str().unwrap_or_default().into(),
            arguments: match &call["function"]["arguments"] {
                Value::String(text) => text.clone(),
                Value::Null => String::new(),
                value => value.to_string(),
            },
        })
        .collect();
    Ok(Reply {
        id: body["id"].as_str().map(str::to_string),
        model: body["model"].as_str().map(str::to_string),
        reasoning: reasoning_of(message).unwrap_or_default().into(),
        text: text_of(&message["content"]),
        refusal: message["refusal"].as_str().unwrap_or_default().into(),
        calls,
        finish_reason: choice["finish_reason"].as_str().map(str::to_string),
        usage: body.get("usage").filter(|usage| usage.is_object()).cloned(),
    }
    .into_response())
}

/// ストリームの断片を最後まで読み、Responses 形式の応答にする。
async fn read_chat_stream(response: Response, on_delta: DeltaSink<'_>) -> Result<Value> {
    let mut events = response.bytes_stream().eventsource();
    let mut reply = Reply::default();
    let mut done = false;
    while let Some(event) = events.next().await {
        let event = event
            .map_err(|error| anyhow!("{error}"))
            .with_context(|| format!("failed to read the streamed OpenAI {ENDPOINT} reply"))?;
        if event.data.trim() == "[DONE]" {
            done = true;
            break;
        }
        // Keep-alives carry no data. Other data that is not JSON would lose
        // part of the answer or of a tool call's arguments if skipped.
        if event.data.trim().is_empty() {
            continue;
        }
        let chunk: Value = serde_json::from_str(&event.data).with_context(|| {
            format!(
                "the streamed OpenAI {ENDPOINT} reply carried invalid JSON: {}",
                truncate(&event.data)
            )
        })?;
        if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
            bail!("OpenAI {ENDPOINT} stream failed: {}", error_message(error));
        }
        if reply.id.is_none() {
            reply.id = chunk["id"].as_str().map(str::to_string);
        }
        if reply.model.is_none() {
            reply.model = chunk["model"].as_str().map(str::to_string);
        }
        if chunk["usage"].is_object() {
            reply.usage = Some(chunk["usage"].clone());
        }
        let Some(choice) = chunk["choices"]
            .as_array()
            .and_then(|choices| choices.first())
        else {
            continue;
        };
        let delta = &choice["delta"];
        if let Some(text) = reasoning_of(delta) {
            reply.reasoning.push_str(text);
            on_delta(ResponseDelta::Reasoning(text));
        }
        for (key, target) in [
            ("content", &mut reply.text),
            ("refusal", &mut reply.refusal),
        ] {
            if let Some(text) = delta[key].as_str().filter(|text| !text.is_empty()) {
                target.push_str(text);
                on_delta(ResponseDelta::Text(text));
            }
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let id = call["id"].as_str().filter(|id| !id.is_empty());
            let index = match call["index"].as_u64() {
                Some(index) => usize::try_from(index).unwrap_or(usize::MAX),
                // 番号のない断片は、新しい id で始まらない限り直前の呼び出しの続き。
                None if id
                    .is_some_and(|id| reply.calls.last().is_none_or(|last| last.id != id)) =>
                {
                    reply.calls.len()
                }
                None => reply.calls.len().saturating_sub(1),
            };
            if index >= reply.calls.len() {
                reply
                    .calls
                    .resize_with(index.min(reply.calls.len() + 64) + 1, Call::default);
            }
            let Some(target) = reply.calls.get_mut(index) else {
                bail!("the streamed {ENDPOINT} reply has an invalid tool call index {index}");
            };
            if let Some(id) = id {
                target.id = id.into();
            }
            if let Some(name) = call["function"]["name"]
                .as_str()
                .filter(|_| target.name.is_empty())
            {
                target.name = name.into();
            }
            match &call["function"]["arguments"] {
                Value::String(arguments) => target.arguments.push_str(arguments),
                Value::Null => {}
                // 断片ではなく、まとめて JSON の値で返すサーバーがある。
                arguments => target.arguments.push_str(&arguments.to_string()),
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            reply.finish_reason = Some(reason.into());
        }
    }
    if !done && reply.finish_reason.is_none() {
        bail!("the OpenAI {ENDPOINT} stream ended before the response was complete");
    }
    if !reply.text.is_empty() || !reply.refusal.is_empty() {
        on_delta(ResponseDelta::MessageDone);
    }
    Ok(reply.into_response())
}

#[cfg(test)]
mod tests;
