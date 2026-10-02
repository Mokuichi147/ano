use super::{
    access::Access, close_sessions, router, serve, session::TurnRefused, startup_message,
    WebOptions, WebState, MAX_SESSIONS, TOKEN_ENV,
};
use crate::{
    application::registry::ToolRegistry, config::AppConfig, domain::tool::ToolDefinition,
    harness::Harness, infrastructure::mcp::McpPool,
};
use axum::{http::StatusCode, routing::post, Json, Router};
use futures::StreamExt;
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use tokio::{sync::Notify, task::JoinSet};

const TOKEN: &str = "secret-token";
/// The cookie that holds the token on the test server.
const COOKIE: &str = "ano_web_1";
/// The host that a browser on another machine names.
const OTHER_HOST: &str = "192.168.1.5:8787";

/// A Responses API that answers by the last input item: "approve" calls the
/// tool that needs approval (after loading it with `tool_search`), "hang"
/// never answers, and anything else gets a Markdown answer.
async fn fake_api(hanging: Arc<Notify>) -> String {
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let hanging = Arc::clone(&hanging);
            async move {
                let last = payload["input"].as_array().unwrap().last().unwrap().clone();
                let text = last["content"][0]["text"].as_str().unwrap_or_default();
                let output = if text == "hang" {
                    hanging.notify_one();
                    return std::future::pending::<Json<Value>>().await;
                } else if text == "approve" {
                    json!([{"type": "function_call", "call_id": "search", "name": "tool_search", "arguments": "{\"query\":\"guarded_tool\"}"}])
                } else if last["call_id"] == "search" {
                    json!([{"type": "function_call", "call_id": "guarded", "name": "guarded_tool", "arguments": "{}"}])
                } else if last["call_id"] == "guarded" {
                    let answer = format!("result: {}", last["output"].as_str().unwrap_or_default());
                    json!([{"type": "message", "content": [{"type": "output_text", "text": answer}]}])
                } else {
                    json!([{"type": "message", "content": [{"type": "output_text", "text": "**done**"}]}])
                };
                Json(json!({"id": "response", "status": "completed", "output": output}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

struct Server {
    url: String,
    state: Arc<WebState>,
    http: reqwest::Client,
    _workspace: tempfile::TempDir,
}

impl Server {
    async fn start(api_url: &str) -> Self {
        let config = AppConfig::parse(&format!(
            "[agent]\nprovider = 'fake'\n[providers.fake]\nbase_url = '{api_url}'\nmodel = 'fake-model'\nstream = false\nmax_retries = 0"
        ))
        .unwrap();
        let registry = ToolRegistry::new();
        registry
            .register(
                ToolDefinition::new(
                    "guarded_tool",
                    "A test tool that needs approval",
                    json!({"type": "object", "properties": {}, "additionalProperties": false}),
                )
                .with_approval(),
                |_| async { Ok(json!("ran")) },
            )
            .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let state = Arc::new(WebState {
            harness: Harness {
                registry,
                history: None,
                skills: None,
            },
            config,
            mcp: Arc::new(McpPool::new(Vec::new())),
            user: "default".into(),
            default_workspace: workspace.path().to_path_buf(),
            access: Access::new(Some(TOKEN.into()), 1),
            sessions: RwLock::default(),
            creating: tokio::sync::Mutex::new(()),
            turns: Mutex::new(JoinSet::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = router(Arc::clone(&state)).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url,
            state,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            _workspace: workspace,
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.url))
            .header("cookie", format!("other=1; {COOKIE}={TOKEN}"))
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.request(reqwest::Method::POST, path)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// The session's logged events, once one of `kind` has arrived.
    async fn wait_for(&self, id: &str, kind: &str, count: usize) -> Vec<Value> {
        let session = self.state.session(id).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let events: Vec<Value> = session
                    .log
                    .subscribe(0)
                    .0
                    .iter()
                    .filter(|event| event.id.is_some())
                    .map(|event| serde_json::from_str(&event.data).unwrap())
                    .collect();
                if events.iter().filter(|event| event["type"] == kind).count() >= count {
                    return events;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {kind} event"))
    }
}

#[tokio::test]
async fn this_machine_needs_no_token_but_other_machines_and_sites_do() {
    let server = Server::start("http://127.0.0.1:9/v1").await;
    let http = &server.http;
    let get = |path: &str, host: &str, cookie: Option<String>| {
        let mut request = http
            .get(format!("{}{path}", server.url))
            .header("host", host.to_string());
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        request.send()
    };

    // A browser on this machine.
    let api = http
        .get(format!("{}/api/sessions", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(api.status(), StatusCode::OK);
    assert_eq!(api.json::<Value>().await.unwrap(), json!([]));
    let page = http.get(format!("{}/", server.url)).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.text().await.unwrap().contains("/assets/main.js"));

    // Another site whose name resolves to this machine (DNS rebinding).
    let rebound = get("/api/sessions", "attacker.example:8787", None)
        .await
        .unwrap();
    assert_eq!(rebound.status(), StatusCode::UNAUTHORIZED);

    // Another machine: the token, then the cookie.
    let locked = get("/", OTHER_HOST, None).await.unwrap();
    assert_eq!(locked.status(), StatusCode::UNAUTHORIZED);
    assert!(locked.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("default-src 'self'"));
    let wrong = get("/?token=nope", OTHER_HOST, None).await.unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    let login = get(&format!("/?token={TOKEN}"), OTHER_HOST, None)
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::SEE_OTHER);
    assert_eq!(login.headers()["location"], "/");
    let cookie = login.headers()["set-cookie"].to_str().unwrap();
    assert!(
        cookie.starts_with(&format!("{COOKIE}={TOKEN};")),
        "{cookie}"
    );
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    let page = get("/", OTHER_HOST, Some(format!("{COOKIE}={TOKEN}")))
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let api = get(
        "/api/sessions",
        OTHER_HOST,
        Some(format!("{COOKIE}={TOKEN}")),
    )
    .await
    .unwrap();
    assert_eq!(api.status(), StatusCode::OK);
    let wrong = get(
        "/api/sessions",
        OTHER_HOST,
        Some(format!("{COOKIE}=secret-tokem")),
    )
    .await
    .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    // A page of another site cannot change anything, token or not.
    let forged = server
        .request(reqwest::Method::POST, "/api/sessions")
        .header("origin", "https://attacker.example")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), StatusCode::FORBIDDEN);
    assert!(server.state.sessions().is_empty());

    for (path, content_type) in [
        ("/assets/main.js", "text/javascript"),
        ("/assets/pkg/ano_web_ui.js", "text/javascript"),
        ("/assets/pkg/ano_web_ui_bg.wasm", "application/wasm"),
    ] {
        let asset = get(path, OTHER_HOST, None).await.unwrap();
        assert_eq!(asset.status(), StatusCode::OK, "{path}");
        assert_eq!(asset.headers()["content-type"], content_type, "{path}");
    }
}

#[tokio::test]
async fn a_session_works_in_its_folder_and_asks_the_page_for_approval() {
    let hanging = Arc::new(Notify::new());
    let server = Server::start(&fake_api(Arc::clone(&hanging)).await).await;
    let options: Value = server
        .request(reqwest::Method::GET, "/api/options")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(options["max_sessions"], MAX_SESSIONS);
    assert_eq!(options["default_preset"]["name"], "default");
    assert_eq!(options["default_approval_mode"], "auto");

    let folder = tempfile::tempdir().unwrap();
    let missing = server
        .post(
            "/api/sessions",
            json!({"workspace": folder.path().join("missing")}),
        )
        .await;
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    assert!(missing.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("workspace does not exist"));

    let created = server
        .post(
            "/api/sessions",
            json!({"workspace": folder.path(), "allow_writes": true, "approval_mode": "ask"}),
        )
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let session: Value = created.json().await.unwrap();
    let id = session["id"].as_str().unwrap().to_string();
    assert_eq!(
        session["workspace"],
        json!(std::fs::canonicalize(folder.path()).unwrap())
    );
    assert_eq!(session["allow_writes"], true);
    assert_eq!(session["running"], false);
    assert!(session["model"].as_str().unwrap().contains("fake-model"));
    let second = server
        .post("/api/sessions", json!({"workspace": folder.path()}))
        .await;
    assert_eq!(second.status(), StatusCode::CONFLICT);

    // A plain turn: the answer arrives as rendered Markdown.
    let sent = server
        .post(
            &format!("/api/sessions/{id}/messages"),
            json!({"text": "hello"}),
        )
        .await;
    assert_eq!(sent.status(), StatusCode::ACCEPTED);
    let events = server.wait_for(&id, "turn_finished", 1).await;
    let message = events
        .iter()
        .find(|event| event["type"] == "message")
        .unwrap();
    assert_eq!(message["html"], "<p><strong>done</strong></p>\n");
    let finished = events.last().unwrap();
    assert_eq!(finished["type"], "turn_finished");
    assert_eq!(finished["outcome"], "completed");
    assert_eq!(finished["usage"]["responses"], 1);

    // A turn whose tool needs approval waits for the page's answer.
    server
        .post(
            &format!("/api/sessions/{id}/messages"),
            json!({"text": "approve"}),
        )
        .await;
    let events = server.wait_for(&id, "approval_requested", 1).await;
    let request = events
        .iter()
        .find(|event| event["type"] == "approval_requested")
        .unwrap();
    assert_eq!(request["target"], "guarded_tool");
    assert_eq!(request["mcp"], false);
    let busy = server
        .post(
            &format!("/api/sessions/{id}/messages"),
            json!({"text": "again"}),
        )
        .await;
    assert_eq!(busy.status(), StatusCode::CONFLICT);
    let status: Value = server
        .request(reqwest::Method::GET, &format!("/api/sessions/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["running"], true);
    let approval = request["id"].as_str().unwrap();
    let answered = server
        .post(
            &format!("/api/sessions/{id}/approvals/{approval}"),
            json!({"approved": true}),
        )
        .await;
    assert_eq!(answered.status(), StatusCode::NO_CONTENT);
    let events = server.wait_for(&id, "turn_finished", 2).await;
    assert!(events
        .iter()
        .any(|event| event["type"] == "approval_resolved" && event["approved"] == true));
    let answer = events
        .iter()
        .rev()
        .find(|event| event["type"] == "message")
        .unwrap();
    assert_eq!(answer["text"], "result: \"ran\"");
    let repeated = server
        .post(
            &format!("/api/sessions/{id}/approvals/{approval}"),
            json!({"approved": true}),
        )
        .await;
    assert_eq!(repeated.status(), StatusCode::NOT_FOUND);

    // A page that reconnects reads what it missed after its last event.
    let last_id = events.len() - 1;
    let stream = server
        .request(reqwest::Method::GET, &format!("/api/sessions/{id}/events"))
        .header("last-event-id", last_id.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(stream.headers()["content-type"], "text/event-stream");
    let mut body = stream.bytes_stream();
    let chunk = String::from_utf8(body.next().await.unwrap().unwrap().to_vec()).unwrap();
    assert!(chunk.contains(&format!("id: {}", last_id + 1)), "{chunk}");
    assert!(chunk.contains("turn_finished"), "{chunk}");
    assert!(!chunk.contains("user_message"), "{chunk}");
    drop(body);

    // Stopping a hanging turn ends it; the conversation goes on.
    server
        .post(
            &format!("/api/sessions/{id}/messages"),
            json!({"text": "hang"}),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(5), hanging.notified())
        .await
        .unwrap();
    let cancelled: Value = server
        .post(&format!("/api/sessions/{id}/cancel"), json!(null))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(cancelled["cancelled"], true);
    let events = server.wait_for(&id, "turn_finished", 3).await;
    assert_eq!(events.last().unwrap()["cancelled"], true);
    server
        .post(
            &format!("/api/sessions/{id}/messages"),
            json!({"text": "hello"}),
        )
        .await;
    server.wait_for(&id, "turn_finished", 4).await;

    // Ending the session makes room for a new one.
    let ended = server
        .request(reqwest::Method::DELETE, &format!("/api/sessions/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(ended.status(), StatusCode::NO_CONTENT);
    let gone = server
        .request(reqwest::Method::GET, &format!("/api/sessions/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    let next = server
        .post("/api/sessions", json!({"workspace": folder.path()}))
        .await;
    assert_eq!(next.status(), StatusCode::CREATED);
    assert_eq!(next.json::<Value>().await.unwrap()["approval_mode"], "auto");
}

#[tokio::test]
async fn shutting_down_ends_the_event_streams_of_open_pages() {
    let server = Server::start("http://127.0.0.1:9/v1").await;
    let created: Value = server
        .post("/api/sessions", json!({}))
        .await
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();
    let mut events = server
        .request(reqwest::Method::GET, &format!("/api/sessions/{id}/events"))
        .send()
        .await
        .unwrap()
        .bytes_stream();

    close_sessions(&server.state).await;

    let mut received = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(chunk) = events.next().await {
            received.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
    })
    .await
    .expect("the event stream stays open");
    assert!(received.contains(r#"{"type":"closed"}"#), "{received}");
    assert!(server.state.sessions().is_empty());
}

#[tokio::test]
async fn ending_a_running_session_stops_its_turn_before_it_reports_the_end() {
    let hanging = Arc::new(Notify::new());
    let server = Server::start(&fake_api(Arc::clone(&hanging)).await).await;
    let created: Value = server
        .post("/api/sessions", json!({}))
        .await
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let session = server.state.session(&id).unwrap();
    server
        .post(
            &format!("/api/sessions/{id}/messages"),
            json!({"text": "hang"}),
        )
        .await;
    tokio::time::timeout(Duration::from_secs(5), hanging.notified())
        .await
        .unwrap();

    let ended = server
        .request(reqwest::Method::DELETE, &format!("/api/sessions/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(ended.status(), StatusCode::NO_CONTENT);

    // The turn had stopped when the deletion was answered, and nothing
    // follows the end.
    let types: Vec<Value> = session
        .log
        .subscribe(0)
        .0
        .iter()
        .map(|event| serde_json::from_str::<Value>(&event.data).unwrap()["type"].clone())
        .collect();
    assert_eq!(
        types[types.len() - 2..],
        [json!("turn_finished"), json!("closed")]
    );
    assert!(session.start_turn("again".into()).err() == Some(TurnRefused::Closed));
    let sent = server
        .post(
            &format!("/api/sessions/{id}/messages"),
            json!({"text": "again"}),
        )
        .await;
    assert_eq!(sent.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_server_needs_a_token_fit_for_urls_and_cookies() {
    let start = |bind: &str, token: &str| {
        serve(
            AppConfig::default(),
            Arc::new(McpPool::new(Vec::new())),
            ToolRegistry::new(),
            WebOptions {
                bind: bind.into(),
                workspace: std::env::temp_dir(),
                user: "default".into(),
                token: Some(token.into()),
                authenticate: true,
            },
        )
    };
    for token in ["", "abc;xyz", "a b", "トークン"] {
        let error = start("127.0.0.1:0", token).await.unwrap_err().to_string();
        assert!(error.contains(TOKEN_ENV), "{error}");
    }
}

#[test]
fn the_printed_urls_need_the_token_only_from_other_machines() {
    let message = |address: &str, token| startup_message(address.parse().unwrap(), token);
    assert_eq!(
        message("127.0.0.1:8787", Some("t")),
        "ano web UI: http://127.0.0.1:8787/"
    );
    assert_eq!(
        message("0.0.0.0:8787", Some("t")),
        "ano web UI: http://127.0.0.1:8787/\nFrom other machines: http://<this machine's address>:8787/?token=t"
    );
    assert_eq!(message("[::]:8787", None), "ano web UI: http://[::1]:8787/");
    assert_eq!(
        message("192.168.1.5:8787", Some("t")),
        "ano web UI: http://192.168.1.5:8787/?token=t"
    );
    assert_eq!(
        message("192.168.1.5:8787", None),
        "ano web UI: http://192.168.1.5:8787/"
    );
}
