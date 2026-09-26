use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;

#[tokio::test]
async fn cli_runs_profile_reads_workspace_and_emits_json() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let captured = Arc::clone(&captured);
            async move {
                let mut requests = captured.lock().unwrap();
                requests.push(payload);
                let output = match requests.len() {
                    1 => json!([{
                        "type": "function_call", "call_id": "search",
                        "name": "tool_search", "arguments": "{\"query\":\"workspace_read\"}"
                    }]),
                    2 => json!([{
                        "type": "function_call", "call_id": "read",
                        "name": "workspace_read", "arguments": "{\"path\":\"note.txt\"}"
                    }]),
                    _ => json!([{
                        "type": "message", "content": [{"type": "output_text", "text": "確認完了"}]
                    }]),
                };
                Json(json!({"id": format!("resp_{}", requests.len()), "status": "completed", "output": output}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config_directory = tempfile::tempdir().unwrap();
    let working_directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(config_directory.path().join("workspace")).unwrap();
    std::fs::write(
        config_directory.path().join("workspace/note.txt"),
        "日本語のメモ",
    )
    .unwrap();
    let config_path = config_directory.path().join("agent.toml");
    std::fs::write(&config_path, "[agent]\nmax_tool_rounds = 3\n[environments.review]\nworkspace = 'workspace'\nmodel = 'mock-profile'\nallowed_tools = ['workspace_read']\n").unwrap();

    let output = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new(env!("CARGO_BIN_EXE_ano"))
            .current_dir(working_directory.path())
            .env("OPENAI_BASE_URL", endpoint)
            .env("OPENAI_API_KEY", "test-fixture-key")
            .arg("--config")
            .arg(&config_path)
            .args([
                "run",
                "--environment",
                "review",
                "--quiet",
                "--json",
                "Read note.txt",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["text"], "確認完了");
    assert_eq!(result["response_id"], "resp_3");
    assert_eq!(result["events"][2]["output"]["content"], "日本語のメモ");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["model"], "mock-profile");
    assert_eq!(requests[0]["tools"].as_array().unwrap().len(), 3);
    assert!(requests[1]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "workspace_read"));
    assert_eq!(requests[2]["tool_choice"], "none");
    assert_eq!(requests[2]["tools"], json!([]));
    let tool_output: Value =
        serde_json::from_str(requests[2]["input"][0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(tool_output["content"], "日本語のメモ");
}

#[tokio::test]
async fn cli_rejects_unknown_user_before_connecting() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    std::fs::write(&config, "").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ano"))
        .current_dir(directory.path())
        .arg("--config")
        .arg(config)
        .args(["--user", "typo", "run", "hello"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown user 'typo'"));
}
