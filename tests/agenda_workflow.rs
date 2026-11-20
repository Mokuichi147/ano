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

/// A run loads `agenda_suggest` with `tool_search` and leaves a plan the
/// user mentioned in the feed, with the time read from the user's words;
/// `ano feed` shows it as an idea that sets up the reminder.
#[tokio::test]
async fn runs_leave_mentioned_plans_as_candidates_in_the_feed() {
    let (endpoint, requests) = mock_responses(vec![
        json!([{"type": "function_call", "call_id": "search", "name": "tool_search", "arguments": "{\"query\":\"agenda_suggest\"}"}]),
        json!([{"type": "function_call", "call_id": "plan", "name": "agenda_suggest",
                "arguments": json!({"title": "歯医者", "when": "2099年1月5日15時", "quote": "1月5日の15時に歯医者"}).to_string()}]),
        json!([{"type": "message", "content": [{"type": "output_text", "text": "noted"}]}]),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[feed]\nenabled = true\ndir = 'feed'\n[agenda]\nenabled = true\n",
    )
    .unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        ano(directory.path())
            .env("OPENAI_BASE_URL", &endpoint)
            .env("OPENAI_API_KEY", "test-fixture-key")
            .args([
                "run",
                "--approval-mode",
                "deny",
                "2099年1月5日の15時に歯医者があるんだ",
            ])
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
    assert_eq!(requests.len(), 3);
    let instructions = requests[0]["instructions"].as_str().unwrap();
    assert!(instructions.contains("# Agenda\n"), "{instructions}");
    let offered = |turn: usize, name: &str| {
        requests[turn]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == name)
    };
    assert!(!offered(0, "agenda_suggest"));
    assert!(offered(1, "agenda_suggest"));
    let result = requests[2]["input"].to_string();
    assert!(
        result.contains("suggested") && result.contains("2099-01-05 15:00"),
        "{result}"
    );

    let listed = ano(directory.path()).args(["feed"]).output().await.unwrap();
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        stdout.contains("[f1] idea 2099-01-05 15:00 歯医者\n      → 2099-01-05 15:00 の「歯医者」のリマインダーを作って"),
        "{stdout}"
    );
}

/// `[agenda]` leaves candidates in the feed, so it needs `[feed]`.
#[tokio::test]
async fn the_agenda_without_the_feed_is_a_config_error() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[agenda]\nenabled = true\n",
    )
    .unwrap();
    let output = ano(directory.path())
        .env("OPENAI_API_KEY", "test-fixture-key")
        .args(["run", "hello"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[feed] enabled = true"), "{stderr}");
}
