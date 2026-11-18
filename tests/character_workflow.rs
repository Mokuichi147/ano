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

/// The character set from the command line is in the next run's
/// instructions; the model changes it with `character_update` through
/// approval, and the change is what `ano character` shows afterwards.
#[tokio::test]
async fn runs_take_on_the_character_and_change_it_with_approval() {
    let (endpoint, requests) = mock_responses(vec![
        json!([{"type": "function_call", "call_id": "update", "name": "character_update",
                "arguments": json!({"tagline": "Always brief."}).to_string()}]),
        json!([{"type": "message", "content": [{"type": "output_text", "text": "done"}]}]),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[character]\nenabled = true\ndir = 'character'\n",
    )
    .unwrap();

    let set = ano(directory.path())
        .args([
            "character",
            "set",
            "--name",
            "Mira",
            "--soul",
            "Speaks warmly.",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        set.status.success(),
        "{}",
        String::from_utf8_lossy(&set.stderr)
    );
    assert!(directory.path().join("character/default/SOUL.md").is_file());

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
                "これからは短く話して",
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
    assert_eq!(requests.len(), 2);
    let instructions = requests[0]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("# Character\nYou are Mira\n")
            && instructions.contains("Speaks warmly."),
        "{instructions}"
    );
    // Every field is optional, which a strict schema cannot express.
    let offered = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "character_update")
        .unwrap();
    assert_eq!(offered["strict"], false, "{offered}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["events"].as_array().unwrap().iter().any(|event| {
            event["type"] == "local_tool_approval"
                && event["name"] == "character_update"
                && event["approved"] == true
        }),
        "{result:#}"
    );

    let shown = ano(directory.path())
        .args(["character"])
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&shown.stdout);
    assert!(
        stdout.starts_with("Mira\nAlways brief.\n\nSpeaks warmly.\n"),
        "{stdout}"
    );
}

/// Without `[character] enabled`, nothing about a character reaches the
/// model, even when a character file exists.
#[tokio::test]
async fn a_disabled_character_stays_out_of_runs() {
    let (endpoint, requests) = mock_responses(vec![json!([
        {"type": "message", "content": [{"type": "output_text", "text": "hi"}]}
    ])])
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[character]\ndir = 'character'\n",
    )
    .unwrap();
    let set = ano(directory.path())
        .args(["character", "set", "--name", "Mira"])
        .output()
        .await
        .unwrap();
    assert!(set.status.success());

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
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = requests.lock().unwrap().clone();
    let instructions = requests[0]["instructions"].as_str().unwrap();
    assert!(!instructions.contains("# Character"), "{instructions}");
    assert!(!requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "character_update"));
}
