mod common;

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
    command.envs(common::isolated_env());
    command
        .current_dir(directory)
        .args(["--config", "config.toml"])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

/// A run loads `feed_read` with `tool_search` (it is not offered up front,
/// to keep requests small), reads the feed, and posts an idea without
/// approval; `ano feed` lists it, rates it, and marks it done.
#[tokio::test]
async fn runs_post_ideas_that_the_user_rates() {
    let (endpoint, requests) = mock_responses(vec![
        json!([{"type": "function_call", "call_id": "search", "name": "tool_search", "arguments": "{\"query\":\"feed_read\"}"}]),
        json!([{"type": "function_call", "call_id": "read", "name": "feed_read", "arguments": "{\"limit\":10}"}]),
        json!([{"type": "function_call", "call_id": "post", "name": "feed_post",
                "arguments": json!({"kind": "idea", "title": "朝の散歩", "body": "よく眠れないと話していたので。", "action": "近所の20分の散歩コースを考えて"}).to_string()}]),
        json!([{"type": "message", "content": [{"type": "output_text", "text": "posted"}]}]),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[feed]\nenabled = true\ndir = 'feed'\n",
    )
    .unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        ano(directory.path())
            .env("OPENAI_BASE_URL", &endpoint)
            .env("OPENAI_API_KEY", "test-fixture-key")
            .args(["run", "--approval-mode", "deny", "何か提案して"])
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
    let instructions = requests[0]["instructions"].as_str().unwrap();
    assert!(instructions.contains("# Feed\n"), "{instructions}");
    let offered = |turn: usize, name: &str| {
        requests[turn]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == name)
    };
    assert!(offered(0, "feed_post") && !offered(0, "feed_read"));
    assert!(offered(1, "feed_read"));
    let read = requests[2]["input"].to_string();
    assert!(read.contains("items"), "{read}");

    let listed = ano(directory.path()).args(["feed"]).output().await.unwrap();
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        stdout.contains("  [f1] idea 朝の散歩\n      → 近所の20分の散歩コースを考えて"),
        "{stdout}"
    );
    for args in [["feed", "rate", "f1", "up"], ["feed", "done", "f1", ""]] {
        let args: Vec<&str> = args.into_iter().filter(|arg| !arg.is_empty()).collect();
        let changed = ano(directory.path()).args(&args).output().await.unwrap();
        assert!(changed.status.success(), "{args:?}");
    }
    let listed = ano(directory.path())
        .args(["feed", "list", "--all"])
        .output()
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).contains("朝の散歩 👍"));
    let listed = ano(directory.path()).args(["feed"]).output().await.unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).contains("(none)"));
}
