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

async fn listed(directory: &std::path::Path) -> String {
    let output = ano(directory).args(["memory"]).output().await.unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A fact added from the command line is in the next run's instructions;
/// the model saves a new fact through approval and forgets an outdated one,
/// and `ano memory` shows the result.
#[tokio::test]
async fn runs_read_remember_and_forget_facts_about_the_user() {
    let (endpoint, requests) = mock_responses(vec![
        json!([
            {"type": "function_call", "call_id": "save", "name": "memory_save",
             "arguments": json!({"fact": "Lives in Osaka."}).to_string()},
            {"type": "function_call", "call_id": "forget", "name": "memory_forget",
             "arguments": json!({"facts": ["Lives in Tokyo."]}).to_string()}
        ]),
        json!([{"type": "message", "content": [{"type": "output_text", "text": "noted"}]}]),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[memory]\nenabled = true\ndir = 'memory'\n",
    )
    .unwrap();
    for fact in ["Prefers green tea.", "Lives in Tokyo."] {
        let added = ano(directory.path())
            .args(["memory", "add", fact])
            .output()
            .await
            .unwrap();
        assert!(added.status.success());
    }

    let output = tokio::time::timeout(
        Duration::from_secs(20),
        ano(directory.path())
            .env("OPENAI_BASE_URL", &endpoint)
            .env("OPENAI_API_KEY", "test-fixture-key")
            .args([
                "run",
                "--approval-mode",
                "allow",
                "--json",
                "大阪に引っ越しました",
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
    let instructions = requests[0]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("# Memory\n")
            && instructions.contains("\n- Prefers green tea.\n- Lives in Tokyo."),
        "{instructions}"
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["events"].as_array().unwrap().iter().any(|event| {
            event["type"] == "local_tool_approval"
                && event["name"] == "memory_save"
                && event["approved"] == true
        }),
        "{result:#}"
    );
    let shown = listed(directory.path()).await;
    assert!(
        shown.contains("  1. Prefers green tea.\n  2. Lives in Osaka.\n"),
        "{shown}"
    );

    let forgot = ano(directory.path())
        .args(["memory", "forget", "1"])
        .output()
        .await
        .unwrap();
    assert!(forgot.status.success());
    let shown = listed(directory.path()).await;
    assert!(
        shown.contains("  1. Lives in Osaka.\n") && !shown.contains("tea"),
        "{shown}"
    );
}

/// Without `[memory] enabled`, nothing remembered reaches the model, even
/// when a memory file exists.
#[tokio::test]
async fn a_disabled_memory_stays_out_of_runs() {
    let (endpoint, requests) = mock_responses(vec![json!([
        {"type": "message", "content": [{"type": "output_text", "text": "hi"}]}
    ])])
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[memory]\ndir = 'memory'\n",
    )
    .unwrap();
    let added = ano(directory.path())
        .args(["memory", "add", "Prefers green tea."])
        .output()
        .await
        .unwrap();
    assert!(added.status.success());

    let output = tokio::time::timeout(
        Duration::from_secs(20),
        ano(directory.path())
            .env("OPENAI_BASE_URL", &endpoint)
            .env("OPENAI_API_KEY", "test-fixture-key")
            .args(["run", "hello"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.status.success());
    let requests = requests.lock().unwrap().clone();
    let instructions = requests[0]["instructions"].as_str().unwrap();
    assert!(!instructions.contains("# Memory"), "{instructions}");
    assert!(!requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"].as_str().unwrap().starts_with("memory_")));
}
