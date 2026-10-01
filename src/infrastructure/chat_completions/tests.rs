use super::*;
use crate::{
    application::{
        agent::{Agent, RunRequest},
        approval::DenyApproval,
        input::InputPart,
        registry::ToolRegistry,
        settings::AgentSettings,
    },
    domain::policy::UserPolicy,
    infrastructure::{
        mcp::McpPool,
        openai::{create_client, ApiSettings, WireApi},
    },
};
use axum::{routing::post, Json, Router};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// `/v1/chat/completions` から `(content-type, body)` を順に返すサーバー。
async fn serve(replies: Vec<(&'static str, String)>) -> Server {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let seen = Arc::clone(&seen);
            let replies = Arc::clone(&replies);
            async move {
                seen.lock().unwrap().push(body);
                let (content_type, body) =
                    replies.lock().unwrap().pop_front().expect("予期しない要求");
                ([("content-type", content_type)], body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        endpoint,
        requests,
        handle,
    }
}

fn client(endpoint: &str, stream: bool) -> Arc<dyn ResponsesApi> {
    create_client(&ApiSettings {
        base_url: endpoint.into(),
        wire_api: WireApi::ChatCompletions,
        stream,
        max_retries: 0,
        ..ApiSettings::default()
    })
    .unwrap()
}

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect();
    body.push_str("data: [DONE]\n\n");
    body
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

#[test]
fn converts_the_history_into_chat_messages() {
    let payload = json!({
        "model": "m",
        "instructions": "be brief",
        "input": [
            {"role": "user", "content": [
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "data:image/png;base64,AA", "detail": "auto"},
            ]},
            {"type": "reasoning", "id": "rs", "summary": [], "content": [{"type": "reasoning_text", "text": "thinking"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "checking"}]},
            {"type": "function_call", "call_id": "c1", "name": "read", "arguments": "{\"path\":\"a\"}"},
            {"type": "function_call", "call_id": "c2", "name": "read", "arguments": "{\"path\":\"b\"}"},
            {"type": "function_call_output", "call_id": "c1", "output": "A"},
            {"type": "function_call_output", "call_id": "c2", "output": "B"},
            // 暗号化された推論だけの項目は送れないので落とす。
            {"type": "reasoning", "encrypted_content": "opaque", "summary": []},
            {"type": "function_call", "call_id": "c3", "name": "list", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c3", "output": "[]"},
            {"role": "user", "content": [{"type": "input_text", "text": "next"}, {"type": "input_text", "text": "step"}]},
        ],
        "tools": [{"type": "function", "name": "read", "description": "Read", "parameters": {"type": "object"}, "strict": false}],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "max_output_tokens": 100,
        "reasoning": {"effort": "high", "summary": "auto"},
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    let request = chat_request(&payload, true).unwrap();
    assert_eq!(
        request,
        json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA", "detail": "auto"}},
                ]},
                {"role": "assistant", "content": "checking", "reasoning_content": "thinking", "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}},
                    {"id": "c2", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"b\"}"}},
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "A"},
                {"role": "tool", "tool_call_id": "c2", "content": "B"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c3", "type": "function", "function": {"name": "list", "arguments": "{}"}},
                ]},
                {"role": "tool", "tool_call_id": "c3", "content": "[]"},
                {"role": "user", "content": "next\n\nstep"},
            ],
            "tools": [{"type": "function", "function": {"name": "read", "description": "Read", "parameters": {"type": "object"}, "strict": false}}],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "max_tokens": 100,
            "reasoning_effort": "high",
            "stream": true,
            "stream_options": {"include_usage": true},
        })
    );
}

#[test]
fn leaves_out_tools_when_none_may_be_called_and_maps_structured_output() {
    let request = chat_request(
        &json!({
            "model": "m",
            "input": "hi",
            "tools": [{"type": "function", "name": "read", "parameters": {}}],
            "tool_choice": "none",
            "parallel_tool_calls": true,
            "text": {"format": {"type": "json_schema", "name": "review", "strict": true, "schema": {"type": "object"}}},
        }),
        false,
    )
    .unwrap();
    assert_eq!(
        request,
        json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "response_format": {"type": "json_schema", "json_schema": {"name": "review", "schema": {"type": "object"}, "strict": true}},
        })
    );
}

#[test]
fn rejects_what_chat_completions_cannot_carry() {
    for (payload, expected) in [
        (
            json!({"model": "m", "input": "hi", "tools": [{"type": "mcp", "server_label": "x"}]}),
            "function tool",
        ),
        (
            json!({"model": "m", "input": "hi", "previous_response_id": "r1"}),
            "previous_response_id",
        ),
        (
            json!({"model": "m", "input": [{"type": "compaction", "encrypted_content": "x"}]}),
            "compaction",
        ),
        (
            json!({"model": "m", "input": [{"role": "user", "content": [{"type": "input_file", "file_id": "f"}]}]}),
            "input_file",
        ),
    ] {
        let error = chat_request(&payload, false).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn converts_a_completion_into_a_response() {
    let response = response_from_chat(&json!({
        "id": "chatcmpl-1",
        "model": "m",
        "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
            "role": "assistant",
            "content": "reading",
            "reasoning_content": "plan",
            "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "read", "arguments": "{}"}}],
        }}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15,
            "completion_tokens_details": {"reasoning_tokens": 2}},
    }))
    .unwrap();
    assert_eq!(response["id"], "chatcmpl-1");
    assert_eq!(response["status"], "completed");
    let output = response["output"].as_array().unwrap();
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output[0]["content"][0]["text"], "plan");
    assert_eq!(output[1]["type"], "message");
    assert_eq!(output[1]["content"][0]["text"], "reading");
    assert_eq!(output[2]["type"], "function_call");
    assert_eq!(output[2]["call_id"], "c1");
    assert_eq!(output[2]["name"], "read");
    assert_eq!(
        response["usage"],
        json!({"input_tokens": 10, "output_tokens": 5, "total_tokens": 15,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 2}})
    );

    // 出力の上限で止まった応答は、tool を実行させないよう未完了とする。
    let cut = response_from_chat(
        &json!({"choices": [{"finish_reason": "length", "message": {"content": "par"}}]}),
    )
    .unwrap();
    assert_eq!(cut["status"], "incomplete");
    assert_eq!(cut["incomplete_details"]["reason"], "max_output_tokens");
    assert!(cut["id"].as_str().unwrap().starts_with("chatcmpl-"));

    let error =
        response_from_chat(&json!({"error": {"message": "context length exceeded"}})).unwrap_err();
    assert!(error.to_string().contains("context length exceeded"));
}

#[tokio::test]
async fn streams_reasoning_text_and_tool_calls() {
    let body = sse(&[
        json!({"id": "chatcmpl-2", "model": "m", "choices": [{"index": 0, "delta": {"role": "assistant", "reasoning_content": "Think"}}]}),
        json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {"content": "こんにちは"}}]}),
        json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {"content": "、世界"}}]}),
        json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "read", "arguments": ""}}]}}]}),
        json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"path\":"}}]}}]}),
        json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"a\"}"}}]}}]}),
        json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 1, "id": "c2", "function": {"name": "list", "arguments": {"all": true}}}]}}]}),
        json!({"id": "chatcmpl-2", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"id": "chatcmpl-2", "choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}}),
    ]);
    let server = serve(vec![("text/event-stream", body)]).await;
    let (deltas, sink) = collect_deltas();
    let response = client(&server.endpoint, true)
        .create_response_streaming(&json!({"model": "m", "input": "hi"}), &sink)
        .await
        .unwrap();
    let sent = server.requests.lock().unwrap()[0].clone();
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"]["include_usage"], true);
    assert_eq!(
        *deltas.lock().unwrap(),
        ["<reasoning:Think>", "こんにちは", "、世界", "<done>"]
    );
    assert_eq!(response["id"], "chatcmpl-2");
    let output = response["output"].as_array().unwrap();
    assert_eq!(output[0]["content"][0]["text"], "Think");
    assert_eq!(output[1]["content"][0]["text"], "こんにちは、世界");
    assert_eq!(output[2]["call_id"], "c1");
    assert_eq!(output[2]["arguments"], "{\"path\":\"a\"}");
    assert_eq!(output[3]["call_id"], "c2");
    assert_eq!(output[3]["name"], "list");
    assert_eq!(output[3]["arguments"], "{\"all\":true}");
    assert_eq!(response["usage"]["total_tokens"], 7);
}

#[tokio::test]
async fn accepts_a_json_reply_to_a_streaming_request_and_requests_json_without_streaming() {
    let completion = json!({"id": "chatcmpl-3", "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "reasoning": "plan", "content": "whole"}}]}).to_string();
    let server = serve(vec![
        ("application/json", completion.clone()),
        ("application/json", completion),
    ])
    .await;
    let (deltas, sink) = collect_deltas();
    let response = client(&server.endpoint, true)
        .create_response_streaming(&json!({"model": "m", "input": "hi"}), &sink)
        .await
        .unwrap();
    assert_eq!(response["output"][1]["content"][0]["text"], "whole");
    assert_eq!(
        *deltas.lock().unwrap(),
        ["<reasoning:plan>", "whole", "<done>"]
    );

    client(&server.endpoint, false)
        .create_response_streaming(&json!({"model": "m", "input": "hi"}), &sink)
        .await
        .unwrap();
    assert!(server.requests.lock().unwrap()[1].get("stream").is_none());
}

#[tokio::test]
async fn stream_errors_and_truncated_streams_fail() {
    for (body, expected) in [
        (
            "data: {\"error\":{\"message\":\"overloaded\"}}\n\n".to_string(),
            "overloaded",
        ),
        (
            "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n".to_string(),
            "ended before",
        ),
        (
            "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}\n\ndata: [DONE]\n\n".to_string(),
            "invalid JSON",
        ),
    ] {
        let server = serve(vec![("text/event-stream", body)]).await;
        let (_, sink) = collect_deltas();
        let error = client(&server.endpoint, true)
            .create_response_streaming(&json!({"model": "m", "input": "hi"}), &sink)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
}

#[tokio::test]
async fn the_agent_runs_tools_through_chat_completions() {
    let call = json!({"id": "first", "choices": [{"finish_reason": "tool_calls", "message": {
        "role": "assistant",
        "content": null,
        "reasoning_content": "search first",
        "tool_calls": [{"id": "search", "type": "function", "function": {"name": "tool_search", "arguments": "{\"query\":\"example\"}"}}],
    }}]});
    let answer = json!({"id": "answer", "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "完了"}}]});
    let server = serve(vec![
        ("application/json", call.to_string()),
        ("application/json", answer.to_string()),
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
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0]["messages"][0]["role"], "system");
    assert_eq!(
        seen[0]["messages"][1],
        json!({"role": "user", "content": "調べて"})
    );
    assert!(seen[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "tool_search"));
    // 2 回目は履歴全体を送り、前回の推論と呼び出し、その結果を含む。
    let messages = seen[1]["messages"].as_array().unwrap();
    assert_eq!(messages[1], seen[0]["messages"][1]);
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["reasoning_content"], "search first");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "search");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "search");
}
