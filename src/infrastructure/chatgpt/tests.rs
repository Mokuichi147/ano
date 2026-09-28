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
    infrastructure::mcp::McpPool,
};
use axum::{extract::Form, http::HeaderMap, response::IntoResponse, routing::post, Json, Router};
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

struct Fixture {
    client: ChatGptClient,
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    refreshes: Arc<Mutex<usize>>,
    server: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture(replies: Vec<(StatusCode, Value)>) -> Fixture {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let refreshes = Arc::new(Mutex::new(0));
    let refreshed = refreshes.clone();
    let app = Router::new()
        .route(
            "/responses",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let seen = seen.clone();
                let replies = replies.clone();
                async move {
                    seen.lock().unwrap().push((headers, body));
                    let (status, response) = replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("予期しない再試行");
                    if !status.is_success() {
                        return (status, Json(response)).into_response();
                    }
                    let mut body = String::new();
                    if let Some(text) = response["output"][0]["content"][0]["text"].as_str() {
                        body.push_str(&format!(
                            "data: {}\n\n",
                            json!({"type":"response.output_text.delta", "delta":text})
                        ));
                    }
                    body.push_str(&format!(
                        "data: {}\n\n",
                        json!({"type":"response.completed", "response":response})
                    ));
                    // 実際の接続先と同じく、SSE を Content-Type なしで返す。
                    axum::response::Response::new(axum::body::Body::from(body))
                }
            }),
        )
        .route(
            "/oauth/token",
            post(move |Form(fields): Form<HashMap<String, String>>| {
                let refreshed = refreshed.clone();
                async move {
                    assert_eq!(fields["refresh_token"], "refresh");
                    *refreshed.lock().unwrap() += 1;
                    Json(json!({"access_token":"new-access", "expires_in":3600}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth.json");
    std::fs::write(&path, json!({
        "access_token":"access", "refresh_token":"refresh", "account_id":"account", "expires_at":u64::MAX
    }).to_string()).unwrap();
    let client = ChatGptClient {
        http: Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        auth: ChatGptAuth::for_test(&path, &url),
        base_url: url,
        max_retries: 0,
        display_stream: true,
    };
    Fixture {
        client,
        requests,
        refreshes,
        server,
        _directory: directory,
    }
}

fn answer() -> Value {
    json!({"id":"answer", "status":"completed", "output":[{
        "type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"完了"}]
    }]})
}

#[tokio::test]
async fn subscription_runs_tools_with_full_history_without_a_session() {
    let call = json!({"type":"function_call", "name":"tool_search", "call_id":"search", "arguments":"{\"query\":\"example\"}"});
    let reasoning =
        json!({"type":"reasoning", "id":"reasoning", "encrypted_content":"opaque", "summary":[]});
    let fixture = fixture(vec![
        (
            StatusCode::OK,
            json!({"id":"first", "status":"completed", "output":[reasoning, call]}),
        ),
        (StatusCode::OK, answer()),
    ])
    .await;
    // Arc 経由でも接続先の履歴要件が引き継がれることを確認する。
    let client = Arc::new(fixture.client.clone());
    let agent = Agent::new(
        client,
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
    let seen = fixture.requests.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for (headers, body) in seen.iter() {
        assert_eq!(headers["authorization"], "Bearer access");
        assert_eq!(headers["chatgpt-account-id"], "account");
        assert_eq!(headers["originator"], "ano");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("previous_response_id").is_none());
    }
    let history = seen[1].1["input"].as_array().unwrap();
    assert_eq!(history[0], seen[0].1["input"][0]);
    assert!(history.contains(&reasoning));
    assert!(history.contains(&call));
    assert!(history
        .iter()
        .any(|item| item["type"] == "function_call_output" && item["call_id"] == "search"));
}

#[tokio::test]
async fn unauthorized_refreshes_once_and_uses_the_new_token() {
    let fixture = fixture(vec![
        (StatusCode::UNAUTHORIZED, json!({})),
        (StatusCode::OK, answer()),
    ])
    .await;
    let response = fixture
        .client
        .create_response(&json!({"model":"test", "input":"hello"}))
        .await
        .unwrap();
    assert_eq!(response, answer());
    assert_eq!(*fixture.refreshes.lock().unwrap(), 1);
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].0["authorization"], "Bearer new-access");
}

#[tokio::test]
async fn repeated_unauthorized_stops_after_one_refresh() {
    let fixture = fixture(vec![
        (StatusCode::UNAUTHORIZED, json!({})),
        (StatusCode::UNAUTHORIZED, json!({})),
    ])
    .await;
    let error = fixture
        .client
        .create_response(&json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("ano auth login"));
    assert_eq!(*fixture.refreshes.lock().unwrap(), 1);
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn rate_limit_retries_are_bounded() {
    let mut fixture = fixture(vec![
        (StatusCode::TOO_MANY_REQUESTS, json!({})),
        (StatusCode::TOO_MANY_REQUESTS, json!({})),
    ])
    .await;
    fixture.client.max_retries = 1;
    let error = fixture
        .client
        .create_response(&json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("利用上限"));
    assert_eq!(fixture.requests.lock().unwrap().len(), 2);
    assert_eq!(*fixture.refreshes.lock().unwrap(), 0);
}

#[tokio::test]
async fn display_stream_false_still_uses_sse_and_delivers_the_answer() {
    let mut fixture = fixture(vec![(StatusCode::OK, answer())]).await;
    fixture.client.display_stream = false;
    let deltas = Mutex::new(String::new());
    fixture
        .client
        .create_response_streaming(&json!({}), &|delta| {
            if let crate::application::ports::ResponseDelta::Text(text) = delta {
                deltas.lock().unwrap().push_str(text);
            }
        })
        .await
        .unwrap();
    assert_eq!(*deltas.lock().unwrap(), "完了");
    assert_eq!(fixture.requests.lock().unwrap()[0].1["stream"], true);
}

#[test]
fn normalizes_subscription_parameters_and_rejects_unsupported_features() {
    let original = json!({"model":"test", "input":"hello", "store":true, "stream":false,
        "max_output_tokens":100, "temperature":0.7, "top_p":1,
        "include":["reasoning.encrypted_content"], "tools":[{"type":"function", "name":"echo"}]});
    let body = subscription_payload(&original).unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["input"][0]["role"], "user");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("temperature").is_none());
    assert!(body.get("top_p").is_none());
    assert_eq!(original["store"], true);
    for payload in [
        json!({"previous_response_id":"r1"}),
        json!({"tools":[{"type":"mcp"}]}),
        json!({"input":[{"content":[{"type":"input_audio"}]}]}),
    ] {
        assert!(subscription_payload(&payload).is_err());
    }
}

#[test]
fn oauth_credentials_cannot_be_sent_to_a_custom_base_url() {
    let settings = ApiSettings {
        base_url: "https://other.example/v1".into(),
        ..Default::default()
    };
    let error = ChatGptClient::from_settings(&settings)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("接続先は固定"));
}

#[tokio::test]
async fn saved_conversation_replays_prior_turns_and_compacts_by_summary() {
    use crate::{domain::session::SessionBinding, infrastructure::session_store::Session};
    let fixture = fixture(vec![
        (StatusCode::OK, answer()),
        (StatusCode::OK, answer()),
        (StatusCode::OK, answer()),
    ])
    .await;
    let agent = Agent::new(
        fixture.client.clone(),
        AgentSettings::default(),
        Arc::new(McpPool::new(vec![])),
        ToolRegistry::new(),
        UserPolicy::default(),
        Arc::new(DenyApproval),
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.json");
    let request = RunRequest::new(vec![InputPart::Text("最初の質問".into())]);
    let binding = SessionBinding::new(&request.context, fixture.client.base_url()).unwrap();
    let mut session = Session::open(&path, binding.clone(), false).unwrap();
    agent.run_in_session(request, &mut session).await.unwrap();
    drop(session);
    let mut session = Session::open(&path, binding, false).unwrap();
    agent
        .run_in_session(
            RunRequest::new(vec![InputPart::Text("続き".into())]),
            &mut session,
        )
        .await
        .unwrap();
    agent.compact_conversation(&mut session).await.unwrap();
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let history = requests[1].1["input"].as_array().unwrap();
    assert_eq!(history[0], requests[0].1["input"][0]);
    assert!(history.contains(&answer()["output"][0]));
    assert!(history
        .iter()
        .any(|item| item["content"][0]["text"] == "続き"));
    assert_eq!(requests[2].1["store"], false);
    assert!(requests[2].1["instructions"]
        .as_str()
        .unwrap()
        .contains("compact"));
}
