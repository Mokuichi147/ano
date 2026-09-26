use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};

/// A chat keeps one conversation across turns, sends reasoning settings and
/// project instructions, and reports reasoning summaries as progress.
#[tokio::test]
async fn chat_carries_history_between_turns_with_project_instructions() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let captured = Arc::clone(&captured);
            async move {
                let mut requests = captured.lock().unwrap();
                requests.push(payload);
                let turn = requests.len();
                Json(json!({
                    "id": format!("resp_{turn}"),
                    "status": "completed",
                    "output": [
                        {"type": "reasoning", "id": format!("rs_{turn}"), "encrypted_content": "opaque",
                         "summary": [{"type": "summary_text", "text": format!("thinking about turn {turn}")}]},
                        {"type": "message", "role": "assistant", "phase": "final_answer",
                         "content": [{"type": "output_text", "text": format!("answer {turn}")}]}
                    ]
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("AGENTS.md"),
        "Always answer in haiku.",
    )
    .unwrap();
    let config = workspace.path().join("config.toml");
    std::fs::write(&config, "[agent]\nreasoning_summary = 'auto'\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ano"))
        .current_dir(workspace.path())
        .env("OPENAI_BASE_URL", &endpoint)
        .env("OPENAI_API_KEY", "test-fixture-key")
        .arg("--config")
        .arg(&config)
        .args(["chat", "--reasoning-effort", "high"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all("最初の質問\n/usage\n\n/unknown\n次の質問\n/exit\nnot sent\n".as_bytes())
        .await
        .unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    server.abort();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stdout.contains("answer 1") && stdout.contains("answer 2"),
        "{stdout}"
    );
    assert!(
        stderr.contains("[reasoning] thinking about turn 1"),
        "{stderr}"
    );
    assert!(stderr.contains("unknown command /unknown"), "{stderr}");
    assert!(stderr.contains("tokens"), "{stderr}");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "/exit must stop before later lines");
    assert_eq!(
        requests[0]["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert!(requests[0]["instructions"]
        .as_str()
        .unwrap()
        .contains("# Project instructions from AGENTS.md\n"));
    assert!(requests[0]["instructions"]
        .as_str()
        .unwrap()
        .ends_with("Always answer in haiku."));

    // The second turn resends the whole first turn, including reasoning.
    let history = requests[1]["input"].as_array().unwrap();
    let texts = history
        .iter()
        .filter_map(|item| {
            item["content"][0]["text"]
                .as_str()
                .or_else(|| item["encrypted_content"].as_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, vec!["最初の質問", "opaque", "answer 1", "次の質問"]);
    assert_eq!(requests[1]["store"], false);
}
