use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::process::Command;

/// Answer each agent request with the next output of `script`, keeping the
/// requests. The last output repeats once the script runs out.
async fn mock_responses(script: Vec<Value>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let (captured, script) = (Arc::clone(&captured), script.clone());
            async move {
                let mut requests = captured.lock().unwrap();
                requests.push(payload);
                let turn = requests.len();
                let output = script[(turn - 1).min(script.len() - 1)].clone();
                Json(json!({"id": format!("resp_{turn}"), "status": "completed", "output": output}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (endpoint, requests)
}

fn ano(directory: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ano"));
    command
        .current_dir(directory)
        .args(["--config", "config.toml"])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

/// A run builds a space with `space_save`, which is offered up front, then
/// loads `space_data_set` with `tool_search` (it is not offered up front, to
/// keep requests small) and refreshes the space's data, keeping its page.
#[tokio::test]
async fn runs_build_spaces_and_load_the_tool_that_refreshes_them() {
    let page = "<!doctype html><html><head><title>t</title></head><body>habits</body></html>";
    let (endpoint, requests) = mock_responses(vec![
        json!([{"type": "function_call", "call_id": "save", "name": "space_save",
                "arguments": json!({"name": "habits", "title": "習慣", "html": page, "data": {"days": 1}}).to_string()}]),
        json!([{"type": "function_call", "call_id": "search", "name": "tool_search", "arguments": "{\"query\":\"space_data_set\"}"}]),
        json!([{"type": "function_call", "call_id": "refresh", "name": "space_data_set",
                "arguments": json!({"name": "habits", "data": {"days": 2}}).to_string()}]),
        json!([{"type": "message", "content": [{"type": "output_text", "text": "done"}]}]),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[spaces]\nenabled = true\ndir = 'spaces'\n",
    )
    .unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        ano(directory.path())
            .env("OPENAI_BASE_URL", &endpoint)
            .env("OPENAI_API_KEY", "test-fixture-key")
            .args(["run", "--approval-mode", "deny", "習慣のトラッカーを作って"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    let instructions = requests[0]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("load space_data_set with tool_search"),
        "{instructions}"
    );
    let offered = |turn: usize, name: &str| {
        requests[turn]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == name)
    };
    assert!(offered(0, "space_save"));
    assert!(!offered(0, "space_read") && !offered(0, "space_data_set"));
    assert!(offered(2, "space_data_set"));
    let refreshed = requests[3]["input"].to_string();
    assert!(!refreshed.contains("tool_not_selected"), "{refreshed}");

    let space = directory.path().join("spaces/default/habits");
    let data: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(space.join("data.json")).unwrap()).unwrap();
    assert_eq!(data, json!({"days": 2}));
    assert_eq!(
        std::fs::read_to_string(space.join("index.html")).unwrap(),
        page
    );
}
