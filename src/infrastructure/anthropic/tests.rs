use super::*;
use crate::{
    application::{
        agent::{Agent, RunRequest},
        approval::DenyApproval,
        input::InputPart,
        registry::ToolRegistry,
        settings::AgentSettings,
    },
    config::AppConfig,
    domain::policy::UserPolicy,
    infrastructure::{
        mcp::McpPool,
        openai::{create_client, is_unavailable, ApiSettings, WireApi},
    },
};
use axum::{
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    headers: Arc<Mutex<Vec<HeaderMap>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// `/v1/messages` から `(status, content-type, body)` を順に返すサーバー。
async fn serve(replies: Vec<(u16, &'static str, String)>) -> Server {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let headers = Arc::new(Mutex::new(Vec::new()));
    let (seen, seen_headers) = (Arc::clone(&requests), Arc::clone(&headers));
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let app = Router::new().route(
        "/v1/messages",
        post(move |header_map: HeaderMap, Json(body): Json<Value>| {
            let (seen, seen_headers) = (Arc::clone(&seen), Arc::clone(&seen_headers));
            let replies = Arc::clone(&replies);
            async move {
                seen.lock().unwrap().push(body);
                seen_headers.lock().unwrap().push(header_map);
                let (status, content_type, body) =
                    replies.lock().unwrap().pop_front().expect("予期しない要求");
                (
                    StatusCode::from_u16(status).unwrap(),
                    [("content-type", content_type)],
                    body,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        endpoint,
        requests,
        headers,
        handle,
    }
}

fn client(endpoint: &str, stream: bool) -> Arc<dyn ResponsesApi> {
    create_client(&ApiSettings {
        base_url: endpoint.into(),
        api_key_env: "ANO_TEST_UNSET_ANTHROPIC_KEY".into(),
        wire_api: WireApi::Anthropic,
        stream,
        max_retries: 0,
        ..ApiSettings::default()
    })
    .unwrap()
}

fn json_reply(message: &Value) -> (u16, &'static str, String) {
    (200, "application/json", message.to_string())
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
    impl Fn(ResponseDelta<'_>) + Send + Sync,
) {
    let deltas = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&deltas);
    (deltas, move |delta| {
        sink.lock().unwrap().push(match delta {
            ResponseDelta::Text(text) => text.to_string(),
            ResponseDelta::MessageDone => "<done>".to_string(),
            ResponseDelta::Reasoning(text) => format!("<reasoning:{text}>"),
        })
    })
}

const LOCAL: Options = Options {
    stream: false,
    cache: false,
};

fn search_tool() -> Value {
    json!({"type": "function", "name": "search", "description": "Search", "strict": true,
        "parameters": {"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"], "additionalProperties": false}})
}

#[test]
fn converts_the_history_into_messages() {
    let payload = json!({
        "model": "claude-opus-5-5",
        "instructions": "Be brief.",
        "input": [
            {"role": "developer", "content": "Answer in Japanese."},
            {"role": "user", "content": [
                {"type": "input_text", "text": "これは何？"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "auto"},
            ]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "調べます"}]},
            {"type": "function_call", "call_id": "c1", "name": "search", "arguments": "{\"q\":\"a\"}"},
            {"type": "function_call", "call_id": "c2", "name": "search", "arguments": "{\"q\":\"b\"}"},
            {"type": "function_call_output", "call_id": "c1", "output": "A"},
            {"type": "function_call_output", "call_id": "c2", "output": ""},
            {"role": "user", "content": "続けて"},
        ],
        "tools": [search_tool()],
        "tool_choice": "none",
        "parallel_tool_calls": false,
        "reasoning": {"effort": "ultra", "summary": "auto"},
        "text": {"format": {"type": "json_schema", "name": "x", "strict": true, "schema": {"type": "object"}}},
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    let request = messages_request(&payload, &LOCAL, true).unwrap().body;
    assert_eq!(
        request,
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": MAX_TOKENS,
            "system": "Be brief.\n\nAnswer in Japanese.",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "これは何？"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                ]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "調べます"},
                    {"type": "tool_use", "id": "c1", "name": "search", "input": {"q": "a"}},
                    {"type": "tool_use", "id": "c2", "name": "search", "input": {"q": "b"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "c1", "content": "A"},
                    {"type": "tool_result", "tool_use_id": "c2"},
                    {"type": "text", "text": "続けて"},
                ]},
            ],
            // 呼び出させない回も定義は送る。
            "tools": [{"name": "search", "description": "Search",
                "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"], "additionalProperties": false}}],
            "tool_choice": {"type": "none"},
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": "max", "format": {"type": "json_schema", "schema": {"type": "object"}}},
        })
    );

    // 呼び出せる回は並列を止める指定を添える。ストリームとキャッシュも。
    let payload = json!({"model": "m", "input": "hi", "tools": [search_tool()],
        "tool_choice": "auto", "parallel_tool_calls": false, "max_output_tokens": 100});
    let options = Options {
        stream: true,
        cache: true,
    };
    let request = messages_request(&payload, &options, true).unwrap().body;
    assert_eq!(
        request["tool_choice"],
        json!({"type": "auto", "disable_parallel_tool_use": true})
    );
    assert_eq!(request["max_tokens"], 100);
    assert_eq!(request["stream"], true);
    assert_eq!(request["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(
        request["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
    );
    assert!(request.get("system").is_none());
    let request = messages_request(&json!({"model": "m", "input": "hi"}), &options, true)
        .unwrap()
        .body;
    assert_eq!(request["max_tokens"], STREAM_MAX_TOKENS);
    assert!(request.get("tools").is_none() && request.get("tool_choice").is_none());
}

#[test]
fn rejects_what_the_messages_api_cannot_carry() {
    for (payload, expected) in [
        (
            json!({"model": "m", "previous_response_id": "r", "input": []}),
            "previous_response_id",
        ),
        (
            json!({"model": "m", "input": [{"type": "input_audio", "input_audio": {"data": "AQI=", "format": "wav"}}]}),
            "音声",
        ),
        (
            json!({"model": "m", "input": [{"type": "compaction", "encrypted_content": "x"}]}),
            "compaction",
        ),
        (
            json!({"model": "m", "input": "hi", "tools": [{"type": "mcp", "server_label": "docs"}]}),
            "function tool",
        ),
    ] {
        let error = messages_request(&payload, &LOCAL, true).err().unwrap();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
    // 他の接続先の暗号化された推論は読めないので送らない。
    let payload = json!({"model": "m", "input": [
        {"role": "user", "content": "hi"},
        {"type": "reasoning", "encrypted_content": "opaque", "summary": []},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "yo"}]},
    ]});
    let request = messages_request(&payload, &LOCAL, true).unwrap().body;
    assert_eq!(
        request["messages"][1]["content"],
        json!([{"type": "text", "text": "yo"}])
    );
}

/// 思考と呼び出しを含む応答。
fn thinking_reply(signature: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": [
            {"type": "thinking", "thinking": "まず検索する", "signature": signature},
            {"type": "text", "text": "検索します"},
            {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {"q": "x"}},
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "cache_creation_input_tokens": 20, "cache_read_input_tokens": 70, "output_tokens": 5},
    })
}

#[test]
fn thinking_blocks_return_unchanged_only_while_what_precedes_them_is_unchanged() {
    let first = json!({"model": "m", "instructions": "i", "tools": [search_tool()],
        "input": [{"role": "user", "content": [{"type": "input_text", "text": "調べて"}]}]});
    let request = messages_request(&first, &LOCAL, true).unwrap();
    let response = request.response(thinking_reply("sig")).unwrap();
    assert_eq!(response["status"], "completed");
    assert_eq!(
        response["usage"],
        json!({"input_tokens": 100, "output_tokens": 5, "total_tokens": 105,
            "input_tokens_details": {"cached_tokens": 70}, "output_tokens_details": {"reasoning_tokens": 0}})
    );
    let output = response["output"].as_array().unwrap();
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[0]["summary"][0]["text"], "まず検索する");
    assert_eq!(output[1]["content"][0]["text"], "検索します");
    assert_eq!(output[2]["call_id"], "toolu_1");
    assert_eq!(output[2]["arguments"], "{\"q\":\"x\"}");

    let mut input = first["input"].as_array().unwrap().clone();
    input.extend(output.iter().cloned());
    input.push(json!({"type": "function_call_output", "call_id": "toolu_1", "output": "found"}));
    let next = json!({"model": "m", "instructions": "i", "tools": [search_tool()], "input": input, "tool_choice": "none"});
    let request = messages_request(&next, &LOCAL, true).unwrap();
    assert!(request.has_thinking);
    assert_eq!(
        request.body["messages"][1]["content"],
        json!([
            {"type": "thinking", "thinking": "まず検索する", "signature": "sig"},
            {"type": "text", "text": "検索します"},
            {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {"q": "x"}},
        ])
    );
    // 次の応答の思考も、その前までの前置きで確かめられる。
    let response = request.response(thinking_reply("sig2")).unwrap();
    input.extend(response["output"].as_array().unwrap().iter().cloned());
    input.push(json!({"type": "function_call_output", "call_id": "toolu_1", "output": "again"}));
    let third =
        json!({"model": "m", "instructions": "i", "tools": [search_tool()], "input": input});
    let messages = messages_request(&third, &LOCAL, true).unwrap().body["messages"].clone();
    assert_eq!(messages[1]["content"][0]["signature"], "sig");
    assert_eq!(messages[3]["content"][0]["signature"], "sig2");

    // tool が増えたり指示が変わったりすると、それより後の思考は送らない。
    for changed in [
        json!({"model": "m", "instructions": "i", "tools": [search_tool(), {"type": "function", "name": "read"}], "input": third["input"]}),
        json!({"model": "m", "instructions": "changed", "tools": [search_tool()], "input": third["input"]}),
    ] {
        let request = messages_request(&changed, &LOCAL, true).unwrap();
        assert!(!request.has_thinking);
        let messages = request.body["messages"].as_array().unwrap();
        assert!(messages.iter().all(|message| message["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block["type"] != "thinking")));
        assert_eq!(messages[1]["content"][0]["type"], "text");
    }
    // 古い履歴が書き換わると（圧縮など）、その後の思考も送らない。
    let mut edited = third["input"].as_array().unwrap().clone();
    edited[0] =
        json!({"role": "user", "content": [{"type": "input_text", "text": "要約: 調べて"}]});
    let request = messages_request(
        &json!({"model": "m", "instructions": "i", "tools": [search_tool()], "input": edited}),
        &LOCAL,
        true,
    )
    .unwrap();
    assert!(!request.has_thinking);
    // 送り直しでは思考をすべて外す。
    let request = messages_request(&third, &LOCAL, false).unwrap();
    assert!(!request.has_thinking);
}

#[test]
fn converts_stop_reasons_and_redacted_thinking() {
    let request = messages_request(&json!({"model": "m", "input": "hi"}), &LOCAL, true).unwrap();
    let response = request
        .response(json!({"id": "m1", "content": [
            {"type": "redacted_thinking", "data": "opaque"},
            {"type": "text", "text": "途中"},
        ], "stop_reason": "max_tokens", "usage": {"input_tokens": 3, "output_tokens": 4}}))
        .unwrap();
    assert_eq!(response["status"], "incomplete");
    assert_eq!(
        response["incomplete_details"]["reason"],
        "max_output_tokens"
    );
    assert_eq!(response["output"][0]["summary"], json!([]));
    let saved: Value =
        serde_json::from_str(response["output"][0]["encrypted_content"].as_str().unwrap()).unwrap();
    assert_eq!(
        saved[BLOCK_KEY],
        json!({"type": "redacted_thinking", "data": "opaque"})
    );
    assert!(saved[PREFIX_KEY].is_string());

    let response = request
        .response(json!({"id": "m2", "content": [], "stop_reason": "refusal"}))
        .unwrap();
    assert_eq!(response["incomplete_details"]["reason"], "content_filter");
    let error = request
        .response(
            json!({"id": "m3", "content": [], "stop_reason": "model_context_window_exceeded"}),
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("context window"));
    let error = request
        .response(
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad"}}),
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("bad"));
}

#[test]
fn a_tool_result_without_its_call_is_sent_as_text() {
    let payload = json!({"model": "m", "input": [
        {"role": "user", "content": "summary of earlier work"},
        {"type": "function_call_output", "call_id": "gone", "output": "late"},
    ]});
    let request = messages_request(&payload, &LOCAL, true).unwrap().body;
    assert_eq!(
        request["messages"][0]["content"][1],
        json!({"type": "text", "text": "[tool result gone]\nlate"})
    );
}

#[test]
fn tool_ids_and_arguments_from_other_providers_are_made_acceptable() {
    let payload = json!({"model": "m", "tools": [search_tool()], "input": [
        {"role": "user", "content": "hi"},
        {"type": "function_call", "call_id": "functions.search:0", "name": "search", "arguments": "{\"q\":"},
        {"type": "function_call_output", "call_id": "functions.search:0", "output": "error"},
    ]});
    let messages = messages_request(&payload, &LOCAL, true).unwrap().body["messages"].clone();
    let call = &messages[1]["content"][0];
    let id = call["id"].as_str().unwrap();
    assert!(id.starts_with("functions_search_0_"), "{id}");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], id);
    assert_eq!(call["input"], json!({"arguments": "{\"q\":"}));
    assert_eq!(tool_id(&json!("toolu_01-A")), json!("toolu_01-A"));

    // tool を定義しない要求では、呼び出しと結果をテキストにする。
    let mut untooled = payload.clone();
    untooled["tools"] = json!([]);
    let messages = messages_request(&untooled, &LOCAL, true).unwrap().body["messages"].clone();
    assert_eq!(messages[1]["content"][0]["type"], "text");
    assert!(messages[2]["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("[tool result functions_search_0_"));
}

#[tokio::test]
async fn streams_thinking_text_and_tool_calls() {
    let body = sse(&[
        json!({"type": "message_start", "message": {"id": "msg_s", "model": "claude-opus-5-5", "content": [],
            "usage": {"input_tokens": 5, "cache_read_input_tokens": 50, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "考え"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "中"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "ping"}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "こんに"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "ちは"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_s", "name": "search", "input": {}}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"q\":"}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"y\"}"}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 30}}),
        json!({"type": "message_stop"}),
    ]);
    let server = serve(vec![(200, "text/event-stream", body)]).await;
    let (deltas, sink) = collect_deltas();
    let response = client(&server.endpoint, true)
        .create_response_streaming(
            &json!({"model": "claude-opus-5-5", "input": "hi", "tools": [search_tool()]}),
            &sink,
        )
        .await
        .unwrap();
    assert_eq!(
        *deltas.lock().unwrap(),
        [
            "<reasoning:>",
            "<reasoning:考え>",
            "<reasoning:中>",
            "こんに",
            "ちは",
            "<done>"
        ]
    );
    assert_eq!(response["id"], "msg_s");
    let output = response["output"].as_array().unwrap();
    let saved: Value =
        serde_json::from_str(output[0]["encrypted_content"].as_str().unwrap()).unwrap();
    assert_eq!(
        saved[BLOCK_KEY],
        json!({"type": "thinking", "thinking": "考え中", "signature": "sig"})
    );
    assert_eq!(output[1]["content"][0]["text"], "こんにちは");
    assert_eq!(output[2]["arguments"], "{\"q\":\"y\"}");
    assert_eq!(response["usage"]["input_tokens"], 55);
    assert_eq!(response["usage"]["output_tokens"], 30);

    let request = &server.requests.lock().unwrap()[0];
    assert_eq!(request["stream"], true);
    assert_eq!(request["max_tokens"], STREAM_MAX_TOKENS);
    // ローカルの接続先にはキャッシュの指定を送らない。
    assert!(request.get("cache_control").is_none());
    let headers = &server.headers.lock().unwrap()[0];
    assert_eq!(headers["anthropic-version"], "2026-10-21");
    assert!(headers.get("authorization").is_none());
    assert!(headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn sends_the_key_as_x_api_key_and_accepts_json_to_a_streaming_request() {
    let server = serve(vec![json_reply(&json!({"id": "m", "content": [{"type": "text", "text": "whole"}], "stop_reason": "end_turn"}))]).await;
    std::env::set_var("ANO_TEST_ANTHROPIC_KEY", "secret");
    let client = create_client(&ApiSettings {
        base_url: server.endpoint.clone(),
        api_key_env: "ANO_TEST_ANTHROPIC_KEY".into(),
        wire_api: WireApi::Anthropic,
        max_retries: 0,
        ..ApiSettings::default()
    })
    .unwrap();
    let (deltas, sink) = collect_deltas();
    let response = client
        .create_response_streaming(&json!({"model": "m", "input": "hi"}), &sink)
        .await
        .unwrap();
    assert_eq!(response["output"][0]["content"][0]["text"], "whole");
    assert_eq!(*deltas.lock().unwrap(), ["whole", "<done>"]);
    let headers = &server.headers.lock().unwrap()[0];
    assert_eq!(headers["x-api-key"], "secret");
    assert!(headers.get("authorization").is_none());
}

#[tokio::test]
async fn stream_errors_and_truncated_streams_fail() {
    for (events, expected, unavailable) in [
        (
            vec![
                json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            ],
            "Overloaded",
            true,
        ),
        (
            vec![
                json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad"}}),
            ],
            "bad",
            false,
        ),
        (
            vec![
                json!({"type": "message_start", "message": {"id": "m", "content": []}}),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            ],
            "ended before",
            false,
        ),
    ] {
        let server = serve(vec![(200, "text/event-stream", sse(&events))]).await;
        let (_, sink) = collect_deltas();
        let error = client(&server.endpoint, true)
            .create_response_streaming(&json!({"model": "m", "input": "hi"}), &sink)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert_eq!(is_unavailable(&error), unavailable, "{error:#}");
    }
    // 529（overloaded）は他の接続先へ回せる。
    let server = serve(vec![(
        529,
        "application/json",
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})
            .to_string(),
    )])
    .await;
    let error = client(&server.endpoint, false)
        .create_response(&json!({"model": "m", "input": "hi"}))
        .await
        .unwrap_err();
    assert!(is_unavailable(&error), "{error:#}");
    assert!(format!("{error:#}").contains("Anthropic messages request failed"));
}

#[tokio::test]
async fn a_request_whose_thinking_is_refused_is_sent_again_without_it() {
    let first = json!({"model": "m", "tools": [search_tool()], "input": [{"role": "user", "content": "hi"}]});
    let request = messages_request(&first, &LOCAL, true).unwrap();
    let response = request.response(thinking_reply("sig")).unwrap();
    let mut input = first["input"].as_array().unwrap().clone();
    input.extend(response["output"].as_array().unwrap().iter().cloned());
    input.push(json!({"type": "function_call_output", "call_id": "toolu_1", "output": "found"}));

    let refused = json!({"type": "error", "error": {"type": "invalid_request_error",
        "message": "messages.1.content.0: thinking block was modified"}});
    let answer = json!({"id": "m", "content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn"});
    let server = serve(vec![
        (400, "application/json", refused.to_string()),
        json_reply(&answer),
    ])
    .await;
    let response = client(&server.endpoint, false)
        .create_response(&json!({"model": "m", "tools": [search_tool()], "input": input}))
        .await
        .unwrap();
    assert_eq!(response["output"][0]["content"][0]["text"], "done");
    let seen = server.requests.lock().unwrap();
    assert_eq!(seen[0]["messages"][1]["content"][0]["type"], "thinking");
    assert_eq!(seen[1]["messages"][1]["content"][0]["type"], "text");
}

#[tokio::test]
async fn the_agent_runs_tools_through_the_messages_api() {
    let call = |id: &str, signature: &str, query: &str| {
        json!({"id": format!("msg_{id}"), "type": "message", "role": "assistant", "model": "m",
            "content": [
                {"type": "thinking", "thinking": "", "signature": signature},
                {"type": "tool_use", "id": format!("toolu_{id}"), "name": "tool_search", "input": {"query": query}},
            ],
            "stop_reason": "tool_use", "usage": {"input_tokens": 10, "output_tokens": 5}})
    };
    let answer = json!({"id": "msg_c", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": "完了"}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 20, "output_tokens": 2}});
    let server = serve(vec![
        json_reply(&call("a", "sig-a", "example")),
        json_reply(&call("b", "sig-b", "other")),
        json_reply(&answer),
    ])
    .await;
    let agent = Agent::new(
        client(&server.endpoint, true),
        AgentSettings::default(),
        Arc::new(McpPool::new(vec![])),
        ToolRegistry::new(),
        UserPolicy::default(),
        Arc::new(DenyApproval),
    );
    let result = agent
        .run(RunRequest::new(vec![InputPart::Text("調べて".into())]))
        .await
        .unwrap();
    assert_eq!(result.text, "完了");

    let seen = server.requests.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen[0]["system"]
        .as_str()
        .is_some_and(|system| !system.is_empty()));
    assert_eq!(
        seen[0]["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "調べて"}]}])
    );
    assert!(seen[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "tool_search"));
    // 毎回履歴全体を送り、前回までの要求をそのまま前置きにする。
    for round in 1..3 {
        let previous = seen[round - 1]["messages"].as_array().unwrap();
        let messages = seen[round]["messages"].as_array().unwrap();
        assert_eq!(&messages[..previous.len()], &previous[..], "round {round}");
        assert_eq!(seen[round]["system"], seen[0]["system"], "round {round}");
        assert_eq!(seen[round]["tools"], seen[0]["tools"], "round {round}");
    }
    // 実際のループでも、思考は呼び出しの前にそのまま戻る。
    let messages = seen[2]["messages"].as_array().unwrap();
    assert_eq!(
        messages[1]["content"],
        json!([
            {"type": "thinking", "thinking": "", "signature": "sig-a"},
            {"type": "tool_use", "id": "toolu_a", "name": "tool_search", "input": {"query": "example"}},
        ])
    );
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_a");
    assert_eq!(messages[3]["content"][0]["signature"], "sig-b");
    assert_eq!(messages[4]["content"][0]["tool_use_id"], "toolu_b");
}

#[tokio::test]
async fn lists_every_page_of_models_and_reads_the_context_window() {
    let app = Router::new()
        .route(
            "/v1/models",
            get(|headers: HeaderMap, axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                assert_eq!(headers["anthropic-version"], "2026-10-21");
                Json(match query.get("after_id").map(String::as_str) {
                    None => json!({"data": [{"id": "claude-opus-5-5"}, {"id": "claude-haiku-5-5"}], "has_more": true, "last_id": "claude-haiku-5-5"}),
                    Some("claude-haiku-5-5") => json!({"data": [{"id": "claude-sonnet-5-5"}], "has_more": false, "last_id": "claude-sonnet-5-5"}),
                    Some(other) => panic!("unexpected page {other}"),
                })
            }),
        )
        .route(
            "/v1/models/{model}",
            get(|axum::extract::Path(model): axum::extract::Path<String>| async move {
                Json(json!({"id": model, "max_input_tokens": 1_000_000, "max_tokens": 128_000}))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = client(&endpoint, true);
    assert_eq!(
        client.list_models().await.unwrap(),
        Some(vec![
            "claude-haiku-5-5".to_string(),
            "claude-opus-5-5".to_string(),
            "claude-sonnet-5-5".to_string(),
        ])
    );
    assert_eq!(
        client.context_window("claude-opus-5-5").await,
        Some(1_000_000)
    );
    handle.abort();
}

#[test]
fn the_anthropic_wire_api_defaults_to_anthropics_endpoint_and_key() {
    let config = AppConfig::parse(
        "[api]\nwire_api = 'anthropic'\n[providers.claude]\nwire_api = 'anthropic'\n[providers.proxy]\nwire_api = 'anthropic'\nbase_url = 'http://127.0.0.1:1234/v1'\napi_key_env = 'PROXY_KEY'\n",
    )
    .unwrap();
    for name in ["api", "claude"] {
        let settings = config.provider_settings(name).unwrap();
        assert_eq!(settings.base_url, "https://api.anthropic.com/v1", "{name}");
        assert_eq!(settings.api_key_env, "ANTHROPIC_API_KEY", "{name}");
        assert_eq!(
            settings.effective_base_url(),
            "https://api.anthropic.com/v1"
        );
    }
    let proxy = config.provider_settings("proxy").unwrap();
    assert_eq!(proxy.base_url, "http://127.0.0.1:1234/v1");
    assert_eq!(proxy.api_key_env, "PROXY_KEY");
    // OpenAI の既定は変わらない。
    let config = AppConfig::parse("[providers.lan]\nbase_url = 'http://127.0.0.1:1/v1'\n").unwrap();
    assert_eq!(config.api.base_url, "https://api.openai.com/v1");
    assert_eq!(
        config.provider_settings("lan").unwrap().api_key_env,
        "OPENAI_API_KEY"
    );

    // Anthropic の公式 endpoint はキーを要する。ChatGPT 認証とは組み合わせられない。
    let mut settings = config.provider_settings("api").unwrap();
    settings.wire_api = WireApi::Anthropic;
    settings.api_key_env = "ANO_TEST_UNSET_ANTHROPIC_KEY".into();
    settings.apply_wire_api_defaults();
    settings.base_url = "https://api.anthropic.com/v1".into();
    let error = create_client(&settings).err().unwrap();
    assert!(format!("{error:#}").contains("ANO_TEST_UNSET_ANTHROPIC_KEY is not set"));
    settings.auth = crate::infrastructure::openai::ApiAuth::Chatgpt;
    assert!(create_client(&settings).is_err());
}
