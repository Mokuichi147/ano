//! Anthropic の Messages API（`POST /v1/messages`）へ接続し、Claude を
//! API キーで使う。要求を Messages 形式へ、応答を Responses 形式へ変換するため、
//! エージェントと会話履歴は接続先の形式を意識しない。
//!
//! Rust の公式 SDK はないので、他のアダプターと同じく HTTP を直接話す。
//!
//! サーバーは応答を保存しないので、毎回履歴全体を送る。思考（`thinking` と
//! `redacted_thinking` のブロック）は、`reasoning` 項目の `encrypted_content`
//! に元のブロックをそのまま保存し、次の要求で同じ位置へ戻す。Claude は
//! 思考ブロックより前の `system`・`tools`・メッセージが作られたときと同じで
//! ないと、そのブロックを受け付けない（tool の追加や履歴の圧縮で変わる）。
//! そこで作られたときの前置きのハッシュも保存し、前置きが変わったブロックは
//! 送らない。
//!
//! `tool_search` で読み込んだ tool は `defer_loading` にして、その検索の結果に
//! `tool_reference` を添える。遅延読み込みの tool は参照されるまで前置きに
//! 含まれないので、tool を読み込んでも `tools` の前置きは変わらず、それまでの
//! 思考ブロックとプロンプトキャッシュが使えたままになる。

use super::openai::{is_anthropic_endpoint, truncate, OpenAiClient, Unavailable};
use crate::{
    application::ports::{replay_deltas, DeltaSink, ResponseDelta, ResponsesApi},
    domain::tool::TOOL_SEARCH_NAME,
};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::{Response, StatusCode};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const ENDPOINT: &str = "messages";
/// `max_output_tokens` がないときの出力の上限。ストリームしない要求は
/// HTTP のタイムアウトに収まるよう小さくする。
const STREAM_MAX_TOKENS: u64 = 64_000;
const MAX_TOKENS: u64 = 16_000;
/// `encrypted_content` に保存した思考ブロックのキー。
const BLOCK_KEY: &str = "anthropic";
const PREFIX_KEY: &str = "prefix";

#[derive(Clone)]
pub struct AnthropicClient {
    /// 送信・再試行・認証ヘッダーは共通のクライアントが受け持つ。
    inner: OpenAiClient,
}

impl AnthropicClient {
    pub fn new(inner: OpenAiClient) -> Self {
        Self { inner }
    }

    async fn complete(
        &self,
        payload: &Value,
        stream: bool,
        on_delta: DeltaSink<'_>,
    ) -> Result<Value> {
        let request = messages_request(payload, &self.options(stream), true)?;
        match self.send(&request, on_delta).await {
            // 思考ブロックを受け付けなかった（前置きの変化を見落とした場合など）。
            // 思考を外して一度だけ送り直す。
            Err(error) if request.has_thinking && rejects_thinking(&error) => {
                let request = messages_request(payload, &self.options(stream), false)?;
                self.send(&request, on_delta).await
            }
            result => result,
        }
    }

    fn options(&self, stream: bool) -> Options {
        Options {
            stream,
            cache: is_anthropic_endpoint(self.inner.base_url()),
        }
    }

    async fn send(&self, request: &Request, on_delta: DeltaSink<'_>) -> Result<Value> {
        let response = self.inner.send(ENDPOINT, &request.body).await?;
        let is_event_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        if !is_event_stream {
            let response = request.response(read_body(response).await?)?;
            replay(&response, on_delta);
            return Ok(response);
        }
        let reply = read_stream(response, on_delta).await?;
        request.response(reply)
    }

    /// `GET /models` を最後のページまで読む。
    async fn models(&self) -> Result<Option<Vec<String>>> {
        let mut models = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut url = reqwest::Url::parse(&format!("{}/models", self.inner.base_url()))
                .context("the base_url is not a valid URL")?;
            url.query_pairs_mut().append_pair("limit", "1000");
            if let Some(after) = &after {
                url.query_pairs_mut().append_pair("after_id", after);
            }
            let request = self.inner.authorize(self.inner.http().get(url));
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) if error.is_connect() || error.is_timeout() => {
                    return Err(error).context(Unavailable("failed to list the models".into()))
                }
                Err(error) => return Err(error).context("failed to list the models"),
            };
            let status = response.status();
            if matches!(
                status,
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ) {
                return Ok(None);
            }
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                bail!("listing the models failed ({status}): {}", truncate(&body));
            }
            let body = read_body(response).await?;
            let page = body["data"]
                .as_array()
                .context("the models response has no data array")?;
            models.extend(
                page.iter()
                    .filter_map(|model| model["id"].as_str().map(str::to_string)),
            );
            match body["last_id"].as_str() {
                Some(last) if body["has_more"] == true && !page.is_empty() => {
                    after = Some(last.to_string())
                }
                _ => break,
            }
        }
        models.sort();
        models.dedup();
        Ok(Some(models))
    }
}

#[async_trait]
impl ResponsesApi for AnthropicClient {
    fn base_url(&self) -> &str {
        self.inner.base_url()
    }

    async fn create_response(&self, payload: &Value) -> Result<Value> {
        self.complete(payload, false, &|_| {}).await
    }

    async fn create_response_streaming(
        &self,
        payload: &Value,
        on_delta: DeltaSink<'_>,
    ) -> Result<Value> {
        self.complete(payload, self.inner.streams(), on_delta).await
    }

    async fn compact_response(&self, _payload: &Value) -> Result<Value> {
        bail!("Anthropic の接続先は /responses/compact に対応していません。agent.compaction = \"auto\" または \"summary\" を使用してください")
    }

    fn requires_full_history(&self) -> bool {
        true
    }

    /// 読み込んだ tool を外すと `tools` が変わり、思考とキャッシュが無効になる。
    fn keeps_loaded_tools(&self) -> bool {
        true
    }

    async fn list_models(&self) -> Result<Option<Vec<String>>> {
        self.models().await
    }

    /// `GET /models/{model}` の `max_input_tokens`。Anthropic 互換のローカル
    /// サーバー（LM Studio など）では、読み込まれた長さを尋ねる。
    async fn context_window(&self, model: &str) -> Option<u64> {
        let url = format!("{}/models/{model}", self.inner.base_url());
        if let Some(tokens) = self
            .inner
            .get_json(url)
            .await
            .and_then(|body| body["max_input_tokens"].as_u64())
            .filter(|tokens| *tokens > 0)
        {
            return Some(tokens);
        }
        if is_anthropic_endpoint(self.inner.base_url()) {
            return None;
        }
        self.inner.loaded_context_length(model).await
    }
}

/// 要求の組み立て方。
struct Options {
    stream: bool,
    /// プロンプトキャッシュを使う（Anthropic の公式 endpoint だけ）。
    cache: bool,
}

/// 送る要求と、応答の思考ブロックに保存する前置きのハッシュ。
struct Request {
    body: Value,
    /// `system`・`tools`・メッセージを読み込んだ状態。
    prefix: Sha256,
    has_thinking: bool,
}

impl Request {
    /// Messages API の応答を Responses 形式にする。
    fn response(&self, message: Value) -> Result<Value> {
        response_from_message(&message, self.prefix.clone())
    }
}

/// 送る前のブロック。思考ブロックは前置きを確かめてから送る。
enum Block {
    Plain(Value),
    Thinking {
        block: Value,
        prefix: Option<String>,
    },
}

struct Turn {
    role: &'static str,
    blocks: Vec<Block>,
}

/// Responses 形式の要求を Messages 形式へ変換する。`keep_thinking` が偽なら
/// 思考ブロックをすべて外す。
fn messages_request(payload: &Value, options: &Options, keep_thinking: bool) -> Result<Request> {
    let payload = payload
        .as_object()
        .context("Responses リクエストはオブジェクトである必要があります")?;
    if payload
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null())
    {
        bail!("Anthropic の接続先では previous_response_id を利用できません。input に会話履歴全体を渡してください");
    }
    let mut system = payload
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_default()
        .to_string();
    let mut tools = payload
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(anthropic_tool)
        .collect::<Result<Vec<_>>>()?;
    let searched = match payload.get("input") {
        Some(Value::Array(items)) => searched_tools(items, &tools),
        _ => Searched::default(),
    };
    // 読み込んだ tool の定義。参照された位置で前置きに数える。
    let mut deferred = BTreeMap::new();
    for tool in &mut tools {
        let name = tool["name"].as_str().unwrap_or_default();
        if searched.names.contains(name) {
            deferred.insert(name.to_string(), tool.clone());
            tool["defer_loading"] = json!(true);
        }
    }

    let mut turns = Vec::new();
    match payload.get("input") {
        Some(Value::String(text)) => push_blocks(
            &mut turns,
            "user",
            vec![Block::Plain(json!({"type": "text", "text": text}))],
        ),
        Some(Value::Array(items)) => {
            for item in items {
                append_item(&mut turns, &mut system, item, &searched)?;
            }
        }
        None | Some(Value::Null) => {}
        Some(_) => bail!("input は文字列または配列で指定してください"),
    }

    let mut prefix = Sha256::new();
    feed(&mut prefix, "system", &json!(system));
    feed(
        &mut prefix,
        "tools",
        &json!(tools
            .iter()
            .filter(|tool| tool["defer_loading"] != true)
            .collect::<Vec<_>>()),
    );
    let mut has_thinking = false;
    let mut messages: Vec<Value> = Vec::new();
    for turn in turns {
        feed_marker(&mut prefix, turn.role);
        let mut content = Vec::new();
        for block in turn.blocks {
            let block = match block {
                Block::Plain(block) => block,
                Block::Thinking {
                    block,
                    prefix: made,
                } => {
                    let unchanged = made.is_some_and(|made| made == digest(&prefix));
                    if !(keep_thinking && unchanged) {
                        continue;
                    }
                    has_thinking = true;
                    block
                }
            };
            feed(&mut prefix, "block", &block);
            for name in references(&block) {
                if let Some(tool) = deferred.get(name) {
                    feed(&mut prefix, "tool", tool);
                }
            }
            content.push(block);
        }
        feed_marker(&mut prefix, "end");
        if content.is_empty() {
            continue;
        }
        match messages.last_mut() {
            Some(last) if last["role"] == turn.role => {
                if let Some(blocks) = last["content"].as_array_mut() {
                    blocks.extend(content);
                }
            }
            _ => messages.push(json!({"role": turn.role, "content": content})),
        }
    }
    let orphans = orphan_results(&mut messages);
    let untooled = tools.is_empty() && tools_as_text(&mut messages);
    if orphans || untooled {
        // 呼び出しや結果をテキストにしたので、前置きは作られたときと違う。
        has_thinking = false;
        strip_thinking(&mut messages);
    }

    let mut request = Map::new();
    request.insert(
        "model".into(),
        payload.get("model").cloned().unwrap_or(Value::Null),
    );
    let max_tokens = payload
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(if options.stream {
            STREAM_MAX_TOKENS
        } else {
            MAX_TOKENS
        });
    request.insert("max_tokens".into(), json!(max_tokens));
    if !system.is_empty() {
        request.insert("system".into(), json!(system));
    }
    request.insert("messages".into(), Value::Array(messages));
    if !tools.is_empty() {
        let mut choice = match payload.get("tool_choice") {
            Some(Value::String(choice)) if choice == "none" => json!({"type": "none"}),
            Some(Value::String(choice)) if choice == "required" => json!({"type": "any"}),
            Some(choice) if choice["type"] == "function" => {
                json!({"type": "tool", "name": choice["name"]})
            }
            _ => json!({"type": "auto"}),
        };
        if payload.get("parallel_tool_calls") == Some(&Value::Bool(false))
            && choice["type"] != "none"
        {
            choice["disable_parallel_tool_use"] = json!(true);
        }
        // 呼び出させない回も定義は送る。履歴の tool_use は定義を要し、
        // 定義を変えると前置きが変わって思考ブロックを送れなくなるため。
        request.insert("tools".into(), Value::Array(tools));
        request.insert("tool_choice".into(), choice);
    }
    let reasoning = payload.get("reasoning");
    if reasoning.is_some_and(|reasoning| reasoning["summary"].is_string()) {
        // 思考の要約を進捗として表示する。既定（"omitted"）では本文が空になる。
        request.insert(
            "thinking".into(),
            json!({"type": "adaptive", "display": "summarized"}),
        );
    }
    let mut output = Map::new();
    if let Some(effort) = reasoning.and_then(|reasoning| reasoning["effort"].as_str()) {
        output.insert("effort".into(), json!(anthropic_effort(effort)));
    }
    if let Some(format) = payload
        .get("text")
        .and_then(|text| text.get("format"))
        .and_then(output_format)
    {
        output.insert("format".into(), format);
    }
    if !output.is_empty() {
        request.insert("output_config".into(), Value::Object(output));
    }
    if options.cache {
        // 毎回履歴全体を送るので、前回までの部分をキャッシュから読ませる。
        request.insert("cache_control".into(), json!({"type": "ephemeral"}));
    }
    if options.stream {
        request.insert("stream".into(), json!(true));
    }
    Ok(Request {
        body: Value::Object(request),
        prefix,
        has_thinking,
    })
}

/// 履歴の 1 項目を、その役割のメッセージのブロックにする。同じ役割が続く
/// 項目は 1 つのメッセージにまとめる（1 つの応答の思考・本文・呼び出しや、
/// 続けて返す複数の tool の結果）。
fn append_item(
    turns: &mut Vec<Turn>,
    system: &mut String,
    item: &Value,
    searched: &Searched,
) -> Result<()> {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    match kind {
        "message" => match item["role"].as_str().unwrap_or("user") {
            "assistant" => push_blocks(turns, "assistant", assistant_blocks(&item["content"])),
            "user" => push_blocks(turns, "user", user_blocks(&item["content"])?),
            "system" | "developer" => {
                let text = text_of(&item["content"]);
                if text.trim().is_empty() {
                    return Ok(());
                }
                if turns.is_empty() {
                    // 会話の前の指示は system に加える。
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(&text);
                } else {
                    push_blocks(
                        turns,
                        "user",
                        vec![Block::Plain(json!({"type": "text", "text": text}))],
                    );
                }
            }
            role => bail!("Anthropic の接続先へ送れないメッセージの role です: {role}"),
        },
        "function_call" => {
            let input = match &item["arguments"] {
                Value::Object(_) => item["arguments"].clone(),
                Value::Null => json!({}),
                // オブジェクトでない引数（壊れた JSON など）は、モデルが
                // 自分の送ったものを見られるよう文字列のまま残す。
                Value::String(text) => serde_json::from_str::<Value>(text)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({"arguments": text})),
                arguments => json!({"arguments": arguments}),
            };
            let id = item.get("call_id").or_else(|| item.get("id"));
            push_blocks(
                turns,
                "assistant",
                vec![Block::Plain(json!({
                    "type": "tool_use",
                    "id": tool_id(id.unwrap_or(&Value::Null)),
                    "name": item["name"],
                    "input": input,
                }))],
            );
        }
        "function_call_output" => {
            let mut result =
                json!({"type": "tool_result", "tool_use_id": tool_id(&item["call_id"])});
            match &item["output"] {
                Value::String(text) if text.is_empty() => {}
                Value::String(text) => result["content"] = json!(text),
                Value::Array(parts) => {
                    result["content"] = json!(user_blocks(&json!(parts))?
                        .into_iter()
                        .filter_map(|block| match block {
                            Block::Plain(block) => Some(block),
                            Block::Thinking { .. } => None,
                        })
                        .collect::<Vec<_>>())
                }
                Value::Null => {}
                output => result["content"] = json!(output.to_string()),
            }
            if let Some(names) = item["call_id"]
                .as_str()
                .and_then(|id| searched.results.get(id))
            {
                // 検索が読み込んだ tool を参照し、定義をここで見せる。API は
                // 参照とテキストを混ぜた結果を受け付けないので、参照だけにする。
                // 結果の名前と説明は、参照した定義に含まれる。
                result["content"] = json!(names
                    .iter()
                    .map(|name| json!({"type": "tool_reference", "tool_name": name}))
                    .collect::<Vec<_>>());
            }
            push_blocks(turns, "user", vec![Block::Plain(result)]);
        }
        "reasoning" => {
            let Some(saved) = item["encrypted_content"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .filter(|saved| saved[BLOCK_KEY].is_object())
            else {
                // 他の接続先の暗号化された推論や、要約だけの推論は読めない。
                return Ok(());
            };
            push_blocks(
                turns,
                "assistant",
                vec![Block::Thinking {
                    block: saved[BLOCK_KEY].clone(),
                    prefix: saved[PREFIX_KEY].as_str().map(str::to_string),
                }],
            );
        }
        "input_audio" => {
            bail!("Anthropic の接続先は音声の入力に対応していません")
        }
        kind => bail!("Anthropic の接続先へ送れない履歴の項目です: {kind}"),
    }
    Ok(())
}

/// 履歴の `tool_search` が読み込んだ tool のうち、この要求の `tools` にあるもの。
#[derive(Default)]
struct Searched {
    /// 検索の呼び出しの id -> 読み込んだ tool の名前（結果の順）。
    results: BTreeMap<String, Vec<String>>,
    /// どれかの検索が読み込んだ tool の名前。
    names: BTreeSet<String>,
}

/// 履歴の `tool_search` の結果を読む。エージェントは読み込んだ tool を同じ会話の
/// 間は外さない（[`ResponsesApi::keeps_loaded_tools`]）ので、参照した tool は
/// その後の要求の `tools` にも残る。結果を読めない検索や、`tools` にない tool は
/// 参照しない。`tools` がすべて遅延読み込みになる場合（API が受け付けない）は
/// 何も遅延させない。
fn searched_tools(items: &[Value], tools: &[Value]) -> Searched {
    let offered: BTreeSet<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    let searches: BTreeSet<&str> = items
        .iter()
        .filter(|item| item["type"] == "function_call" && item["name"] == TOOL_SEARCH_NAME)
        .filter_map(|item| item["call_id"].as_str())
        .collect();
    let mut searched = Searched::default();
    for item in items {
        let Some(id) = item["call_id"]
            .as_str()
            .filter(|id| item["type"] == "function_call_output" && searches.contains(id))
        else {
            continue;
        };
        let Some(results) = item["output"]
            .as_str()
            .and_then(|output| serde_json::from_str::<Value>(output).ok())
            .and_then(|output| output["tools"].as_array().cloned())
        else {
            continue;
        };
        let mut names = Vec::new();
        for result in &results {
            // 直接つなぐ MCP の tool は関数名で送られる。
            let Some(name) = result["function_name"]
                .as_str()
                .or_else(|| result["name"].as_str())
                .filter(|name| offered.contains(name) && *name != TOOL_SEARCH_NAME)
            else {
                continue;
            };
            if !names.iter().any(|known| known == name) {
                names.push(name.to_string());
            }
        }
        if !names.is_empty() {
            searched.names.extend(names.iter().cloned());
            searched.results.insert(id.to_string(), names);
        }
    }
    if offered.iter().all(|name| searched.names.contains(*name)) {
        return Searched::default();
    }
    searched
}

/// `tool_result` が参照する tool の名前。
fn references(block: &Value) -> impl Iterator<Item = &str> {
    block["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "tool_reference")
        .filter_map(|part| part["tool_name"].as_str())
}

/// Anthropic の tool の id に使えるのは英数字と `_`・`-` だけ。ほかの接続先が
/// 作った id（`functions.search:0` など）は、使えない文字を `_` にし、
/// 元の id のハッシュを添えて区別する。呼び出しと結果は同じ id になる。
fn tool_id(id: &Value) -> Value {
    let Some(id) = id.as_str() else {
        return id.clone();
    };
    let valid = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if !id.is_empty() && id.chars().all(valid) {
        return json!(id);
    }
    let safe: String = id.chars().map(|c| if valid(c) { c } else { '_' }).collect();
    let hash = hex::encode(Sha256::digest(id.as_bytes()));
    json!(format!("{safe}_{}", &hash[..8]))
}

/// tool を定義しない要求では、履歴の tool の呼び出しと結果を送れないので
/// テキストにする。何か変えたら真。
fn tools_as_text(messages: &mut [Value]) -> bool {
    let mut changed = false;
    for block in messages
        .iter_mut()
        .flat_map(|message| message["content"].as_array_mut().into_iter().flatten())
    {
        let text = match block["type"].as_str() {
            Some("tool_use") => format!(
                "[tool call {} {}]\n{}",
                block["name"].as_str().unwrap_or_default(),
                block["id"].as_str().unwrap_or_default(),
                block["input"]
            ),
            Some("tool_result") => format!(
                "[tool result {}]\n{}",
                block["tool_use_id"].as_str().unwrap_or_default(),
                match &block["content"] {
                    Value::String(text) => text.clone(),
                    content => text_of(content),
                }
            ),
            _ => continue,
        };
        *block = json!({"type": "text", "text": text});
        changed = true;
    }
    changed
}

fn push_blocks(turns: &mut Vec<Turn>, role: &'static str, blocks: Vec<Block>) {
    if blocks.is_empty() {
        return;
    }
    match turns.last_mut() {
        Some(last) if last.role == role => last.blocks.extend(blocks),
        _ => turns.push(Turn { role, blocks }),
    }
}

/// assistant メッセージの本文。空のテキストは送れないので外す。
fn assistant_blocks(content: &Value) -> Vec<Block> {
    let parts = match content {
        Value::String(text) => vec![text.as_str()],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str().or_else(|| part["refusal"].as_str()))
            .collect(),
        _ => Vec::new(),
    };
    parts
        .into_iter()
        .filter(|text| !text.is_empty())
        .map(|text| Block::Plain(json!({"type": "text", "text": text})))
        .collect()
}

/// user メッセージや tool の結果の本文（テキストと画像）。
fn user_blocks(content: &Value) -> Result<Vec<Block>> {
    let Value::Array(parts) = content else {
        let text = text_of(content);
        if text.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(vec![Block::Plain(json!({"type": "text", "text": text}))]);
    };
    let mut blocks = Vec::new();
    for part in parts {
        let block = match part["type"].as_str().unwrap_or_default() {
            "input_text" | "output_text" | "text" => {
                let text = part["text"].as_str().unwrap_or_default();
                if text.is_empty() {
                    continue;
                }
                json!({"type": "text", "text": text})
            }
            "input_image" => {
                let url = part["image_url"]
                    .as_str()
                    .context("Anthropic の接続先では URL かデータ URL の画像だけを送れます")?;
                json!({"type": "image", "source": image_source(url)?})
            }
            "input_audio" => bail!("Anthropic の接続先は音声の入力に対応していません"),
            kind => bail!("Anthropic の接続先へ送れない入力の種類です: {kind}"),
        };
        blocks.push(Block::Plain(block));
    }
    Ok(blocks)
}

/// `data:image/png;base64,...` か `https://...` を画像の source にする。
fn image_source(url: &str) -> Result<Value> {
    if let Some(data_url) = url.strip_prefix("data:") {
        let (header, data) = data_url
            .split_once(',')
            .context("画像のデータ URL の形式が正しくありません")?;
        let media_type = header
            .strip_suffix(";base64")
            .context("画像のデータ URL は base64 で指定してください")?;
        return Ok(json!({"type": "base64", "media_type": media_type, "data": data}));
    }
    if url.starts_with("https://") || url.starts_with("http://") {
        return Ok(json!({"type": "url", "url": url}));
    }
    bail!("Anthropic の接続先では URL かデータ URL の画像だけを送れます")
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

/// 直前の assistant メッセージに呼び出しのない tool の結果を、テキストにする。
/// 履歴の圧縮などで呼び出しが消えると、API はその結果を受け付けない。
/// 何か変えたら真。
fn orphan_results(messages: &mut [Value]) -> bool {
    let mut changed = false;
    let mut calls: Vec<Value> = Vec::new();
    for message in messages.iter_mut() {
        if message["role"] == "assistant" {
            calls = message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "tool_use")
                .map(|block| block["id"].clone())
                .collect();
            continue;
        }
        for block in message["content"].as_array_mut().into_iter().flatten() {
            if block["type"] != "tool_result" || calls.contains(&block["tool_use_id"]) {
                continue;
            }
            let output = match &block["content"] {
                Value::String(text) => text.clone(),
                Value::Null => String::new(),
                content => text_of(content),
            };
            let call = block["tool_use_id"].as_str().unwrap_or_default();
            *block = json!({"type": "text", "text": format!("[tool result {call}]\n{output}")});
            changed = true;
        }
        calls.clear();
    }
    changed
}

fn strip_thinking(messages: &mut Vec<Value>) {
    for message in messages.iter_mut() {
        if let Some(blocks) = message["content"].as_array_mut() {
            blocks.retain(|block| !is_thinking(block));
        }
    }
    messages.retain(|message| {
        message["content"]
            .as_array()
            .is_none_or(|blocks| !blocks.is_empty())
    });
}

fn is_thinking(block: &Value) -> bool {
    matches!(
        block["type"].as_str(),
        Some("thinking" | "redacted_thinking")
    )
}

fn anthropic_tool(tool: &Value) -> Result<Value> {
    if tool["type"] != "function" {
        bail!(
            "Anthropic の接続先は function tool だけに対応しています（{} は使えません）。MCP は transport = \"stdio\" または \"streamable_http\" で ano から直接接続してください",
            tool["type"].as_str().unwrap_or("種類なし")
        );
    }
    let mut definition = Map::new();
    definition.insert("name".into(), tool["name"].clone());
    if let Some(description) = tool["description"].as_str().filter(|text| !text.is_empty()) {
        definition.insert("description".into(), json!(description));
    }
    let schema = tool
        .get("parameters")
        .filter(|schema| schema.is_object())
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    definition.insert("input_schema".into(), schema);
    Ok(Value::Object(definition))
}

/// ano の推論の深さを Anthropic の effort にする。Claude には思考しない段階が
/// ないので、none と minimal は最も浅い low にする。
fn anthropic_effort(effort: &str) -> &str {
    match effort {
        "none" | "minimal" => "low",
        "ultra" => "max",
        effort => effort,
    }
}

/// `text.format` の JSON Schema を `output_config.format` にする。
fn output_format(format: &Value) -> Option<Value> {
    match format["type"].as_str()? {
        "json_schema" => Some(json!({"type": "json_schema", "schema": format.get("schema")?})),
        _ => None,
    }
}

/// 前置きのハッシュに値を加える。種類の印で区切り、境目をずらしても同じ
/// ハッシュにならないようにする。
fn feed(hasher: &mut Sha256, marker: &str, value: &Value) {
    feed_marker(hasher, marker);
    let text = value.to_string();
    hasher.update((text.len() as u64).to_le_bytes());
    hasher.update(text.as_bytes());
}

fn feed_marker(hasher: &mut Sha256, marker: &str) {
    hasher.update([0u8]);
    hasher.update(marker.as_bytes());
}

fn digest(hasher: &Sha256) -> String {
    hex::encode(hasher.clone().finalize())
}

/// Messages API の応答を Responses 形式にする。`prefix` は要求を読み込んだ
/// ハッシュで、思考ブロックごとにその前までの前置きを記録する。
fn response_from_message(message: &Value, prefix: Sha256) -> Result<Value> {
    if message["type"] == "error" || message["error"].is_object() {
        bail!(
            "Anthropic {ENDPOINT} request failed: {}",
            error_message(&message["error"])
        );
    }
    let content = message["content"].as_array().with_context(|| {
        format!(
            "the {ENDPOINT} response has no content: {}",
            truncate(&message.to_string())
        )
    })?;
    let stop_reason = message["stop_reason"].as_str();
    if stop_reason == Some("model_context_window_exceeded") {
        bail!("Anthropic {ENDPOINT}: the model's context window was exceeded (model_context_window_exceeded)");
    }
    let id = message["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("msg_{}", uuid::Uuid::new_v4().simple()));

    let mut output: Vec<Value> = Vec::new();
    for (index, block) in content.iter().enumerate() {
        match block["type"].as_str().unwrap_or_default() {
            "text" => {
                let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) else {
                    continue;
                };
                let part = json!({"type": "output_text", "text": text, "annotations": []});
                match output.last_mut() {
                    Some(last) if last["type"] == "message" => {
                        if let Some(parts) = last["content"].as_array_mut() {
                            parts.push(part);
                        }
                    }
                    _ => output.push(json!({
                        "type": "message",
                        "id": format!("{id}_{index}"),
                        "role": "assistant",
                        "status": "completed",
                        "content": [part],
                    })),
                }
            }
            "thinking" | "redacted_thinking" => {
                let summary = block["thinking"]
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| vec![json!({"type": "summary_text", "text": text})])
                    .unwrap_or_default();
                output.push(json!({
                    "type": "reasoning",
                    "id": format!("rs_{id}_{index}"),
                    "summary": summary,
                    "encrypted_content": json!({BLOCK_KEY: block}).to_string(),
                }));
            }
            "tool_use" => {
                let call_id = block["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("toolu_{}", uuid::Uuid::new_v4().simple()));
                let input = if block["input"].is_object() {
                    block["input"].clone()
                } else {
                    json!({})
                };
                output.push(json!({
                    "type": "function_call",
                    "id": format!("fc_{call_id}"),
                    "call_id": call_id,
                    "name": block["name"],
                    "arguments": input.to_string(),
                    "status": "completed",
                }));
            }
            // サーバー側の tool などは ano が扱わない。
            _ => {}
        }
    }
    record_prefixes(&mut output, prefix)?;

    let incomplete = match stop_reason {
        Some("max_tokens") => Some("max_output_tokens"),
        Some("refusal") => Some("content_filter"),
        _ => None,
    };
    let mut response = json!({
        "id": id,
        "object": "response",
        "model": message["model"],
        "status": if incomplete.is_some() { "incomplete" } else { "completed" },
        "output": output,
    });
    if let Some(reason) = incomplete {
        response["incomplete_details"] = json!({"reason": reason});
    }
    if let Some(usage) = usage_from_message(&message["usage"]) {
        response["usage"] = usage;
    }
    Ok(response)
}

/// 応答の思考ブロックに、次の要求で同じ位置に来たときの前置きのハッシュを
/// 記録する。次の要求と同じ変換で assistant メッセージを組み立てて数える。
fn record_prefixes(output: &mut [Value], mut prefix: Sha256) -> Result<()> {
    let mut turns = Vec::new();
    let mut system = String::new();
    for item in output.iter() {
        append_item(&mut turns, &mut system, item, &Searched::default())?;
    }
    let mut thinking = output.iter_mut().filter(|item| item["type"] == "reasoning");
    for turn in turns {
        feed_marker(&mut prefix, turn.role);
        for block in turn.blocks {
            let block = match block {
                Block::Plain(block) => block,
                Block::Thinking { block, .. } => {
                    if let Some(item) = thinking.next() {
                        item["encrypted_content"] = json!(
                            json!({BLOCK_KEY: block, PREFIX_KEY: digest(&prefix)}).to_string()
                        );
                    }
                    block
                }
            };
            feed(&mut prefix, "block", &block);
        }
    }
    Ok(())
}

/// Messages API の使用量を Responses 形式にする。Anthropic の `input_tokens`
/// はキャッシュから読んだ分と書いた分を含まないので足す。
fn usage_from_message(usage: &Value) -> Option<Value> {
    let fresh = usage["input_tokens"].as_u64()?;
    let written = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    let read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let input = fresh.saturating_add(written).saturating_add(read);
    let output = usage["output_tokens"].as_u64().unwrap_or(0);
    Some(json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": input.saturating_add(output),
        "input_tokens_details": {"cached_tokens": read},
        "output_tokens_details": {"reasoning_tokens": 0},
    }))
}

fn error_message(error: &Value) -> String {
    error["message"]
        .as_str()
        .or_else(|| error.as_str())
        .map(truncate)
        .unwrap_or_else(|| truncate(&error.to_string()))
}

/// 400 の理由が思考ブロックにあるか。
fn rejects_thinking(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("(400") && text.to_lowercase().contains("thinking")
}

async fn read_body(response: Response) -> Result<Value> {
    let body = response
        .text()
        .await
        .with_context(|| format!("failed to read Anthropic {ENDPOINT} response"))?;
    serde_json::from_str::<Value>(&body).with_context(|| {
        format!(
            "Anthropic returned invalid JSON for {ENDPOINT}: {}",
            truncate(&body)
        )
    })
}

/// ストリームしなかった応答の推論と本文を `on_delta` に渡す。
fn replay(response: &Value, on_delta: DeltaSink<'_>) {
    for item in response["output"].as_array().into_iter().flatten() {
        if item["type"] != "reasoning" {
            continue;
        }
        for part in item["summary"].as_array().into_iter().flatten() {
            if let Some(text) = part["text"].as_str() {
                on_delta(ResponseDelta::Reasoning(text));
            }
        }
    }
    replay_deltas(response, on_delta);
}

/// ストリームのイベントを最後まで読み、Messages API の応答の形にまとめる。
async fn read_stream(response: Response, on_delta: DeltaSink<'_>) -> Result<Value> {
    let mut events = response.bytes_stream().eventsource();
    let mut message = json!({"content": []});
    let mut blocks: Vec<Value> = Vec::new();
    let mut inputs: Vec<String> = Vec::new();
    let mut in_text = false;
    let mut finished = false;
    while let Some(event) = events.next().await {
        let event = event
            .map_err(|error| anyhow!("{error}"))
            .with_context(|| format!("failed to read the streamed Anthropic {ENDPOINT} reply"))?;
        if event.data.trim().is_empty() {
            continue;
        }
        let data: Value = serde_json::from_str(&event.data).with_context(|| {
            format!(
                "the streamed Anthropic {ENDPOINT} reply carried invalid JSON: {}",
                truncate(&event.data)
            )
        })?;
        let kind = data["type"].as_str().unwrap_or(event.event.as_str());
        match kind {
            "message_start" => {
                let start = &data["message"];
                for key in ["id", "model"] {
                    message[key] = start[key].clone();
                }
                merge_usage(&mut message, &start["usage"]);
            }
            "content_block_start" => {
                let index = block_index(&data, blocks.len())?;
                let mut block = data["content_block"].clone();
                let kind = block["type"].as_str().unwrap_or_default().to_string();
                if kind != "text" && in_text {
                    on_delta(ResponseDelta::MessageDone);
                    in_text = false;
                }
                match kind.as_str() {
                    "text" => {
                        if let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) {
                            on_delta(ResponseDelta::Text(text));
                            in_text = true;
                        }
                    }
                    // 思考を始めたことを知らせる（既定では本文が空のまま進む）。
                    "thinking" | "redacted_thinking" => on_delta(ResponseDelta::Reasoning(
                        block["thinking"].as_str().unwrap_or_default(),
                    )),
                    "tool_use" => block["input"] = json!({}),
                    _ => {}
                }
                if index >= blocks.len() {
                    blocks.resize(index + 1, Value::Null);
                    inputs.resize(index + 1, String::new());
                }
                blocks[index] = block;
            }
            "content_block_delta" => {
                let index = block_index(&data, blocks.len())?;
                let Some(block) = blocks.get_mut(index) else {
                    bail!(
                        "the streamed {ENDPOINT} reply has a delta for the unknown block {index}"
                    );
                };
                let delta = &data["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let text = delta["text"].as_str().unwrap_or_default();
                        append(block, "text", text);
                        if !text.is_empty() {
                            on_delta(ResponseDelta::Text(text));
                            in_text = true;
                        }
                    }
                    "thinking_delta" => {
                        let text = delta["thinking"].as_str().unwrap_or_default();
                        append(block, "thinking", text);
                        on_delta(ResponseDelta::Reasoning(text));
                    }
                    "signature_delta" => append(
                        block,
                        "signature",
                        delta["signature"].as_str().unwrap_or_default(),
                    ),
                    "input_json_delta" => {
                        inputs[index].push_str(delta["partial_json"].as_str().unwrap_or_default())
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = block_index(&data, blocks.len())?;
                if blocks
                    .get(index)
                    .is_some_and(|block| block["type"] == "tool_use")
                    && !inputs[index].trim().is_empty()
                {
                    let input: Value = serde_json::from_str(&inputs[index]).with_context(|| {
                        format!(
                            "the streamed {ENDPOINT} reply carried invalid tool input: {}",
                            truncate(&inputs[index])
                        )
                    })?;
                    blocks[index]["input"] = input;
                }
            }
            "message_delta" => {
                for key in ["stop_reason", "stop_details"] {
                    if !data["delta"][key].is_null() {
                        message[key] = data["delta"][key].clone();
                    }
                }
                merge_usage(&mut message, &data["usage"]);
            }
            "message_stop" => {
                finished = true;
                break;
            }
            "error" => {
                let error = &data["error"];
                let text = format!(
                    "Anthropic {ENDPOINT} stream failed: {}",
                    error_message(error)
                );
                if matches!(
                    error["type"].as_str(),
                    Some("overloaded_error" | "api_error")
                ) {
                    return Err(Unavailable(text).into());
                }
                bail!(text);
            }
            _ => {}
        }
    }
    if !finished {
        bail!("the Anthropic {ENDPOINT} stream ended before the response was complete");
    }
    if in_text {
        on_delta(ResponseDelta::MessageDone);
    }
    message["content"] = Value::Array(
        blocks
            .into_iter()
            .filter(|block| !block.is_null())
            .collect(),
    );
    Ok(message)
}

fn block_index(data: &Value, len: usize) -> Result<usize> {
    let index = data["index"]
        .as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .context("the streamed reply has a content block without an index")?;
    if index > len + 64 {
        bail!("the streamed {ENDPOINT} reply has an invalid block index {index}");
    }
    Ok(index)
}

fn append(block: &mut Value, key: &str, text: &str) {
    let mut joined = block[key].as_str().unwrap_or_default().to_string();
    joined.push_str(text);
    block[key] = json!(joined);
}

/// ストリームの使用量は `message_start` と `message_delta` に分かれて届く。
fn merge_usage(message: &mut Value, usage: &Value) {
    let Some(usage) = usage.as_object() else {
        return;
    };
    if !message["usage"].is_object() {
        message["usage"] = json!({});
    }
    for (key, value) in usage {
        if !value.is_null() {
            message["usage"][key] = value.clone();
        }
    }
}

#[cfg(test)]
mod tests;
