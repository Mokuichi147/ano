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

/// With `[memory] enabled = false`, nothing remembered reaches the model,
/// even when a memory file exists.
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
    std::fs::write(
        directory.path().join("config.toml"),
        "[memory]\nenabled = false\ndir = 'memory'\n",
    )
    .unwrap();

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

/// `ano memory consolidate` shows the merges the model proposes with
/// `--dry-run`, and without it rewrites the memory and keeps the file as it
/// was; a proposal that is not sound is reported and left out.
#[tokio::test]
async fn consolidation_merges_the_facts_the_model_names_and_keeps_a_backup() {
    let merges = json!({"merges": [
        {"facts": [1, 3], "into": "Prefers green tea to coffee."},
        {"facts": [2, 9], "into": "Names a fact that does not exist."}
    ]});
    let (endpoint, requests) = mock_responses(vec![json!([
        {"type": "message", "content": [{"type": "output_text", "text": merges.to_string()}]}
    ])])
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[memory]\nenabled = true\ndir = 'memory'\n",
    )
    .unwrap();
    for fact in [
        "Likes green tea.",
        "Has a cat named Mochi.",
        "Prefers green tea to coffee.",
    ] {
        let added = ano(directory.path())
            .args(["memory", "add", fact])
            .output()
            .await
            .unwrap();
        assert!(added.status.success());
    }
    let consolidate = async |extra: &[&str]| {
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            ano(directory.path())
                .env("OPENAI_BASE_URL", &endpoint)
                .env("OPENAI_API_KEY", "test-fixture-key")
                .args(["memory", "consolidate"])
                .args(extra)
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
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };

    let (shown, _) = consolidate(&["--dry-run"]).await;
    assert!(
        shown.contains("  1. Likes green tea.\n  3. Prefers green tea to coffee.\n  -> Prefers green tea to coffee.\n")
            && shown.contains("Would consolidate 3 facts into 2"),
        "{shown}"
    );
    assert!(listed(directory.path()).await.contains("  3. Prefers"));

    let (shown, warnings) = consolidate(&[]).await;
    assert!(shown.contains("Consolidated 3 facts into 2."), "{shown}");
    assert!(
        warnings.contains("skipped a proposed merge of 2, 9"),
        "{warnings}"
    );
    let listed = listed(directory.path()).await;
    assert!(
        listed.contains("  1. Has a cat named Mochi.\n  2. Prefers green tea to coffee.\n"),
        "{listed}"
    );
    let backup =
        std::fs::read_to_string(directory.path().join("memory/default/MEMORY.md.bak")).unwrap();
    assert!(backup.contains("- Likes green tea.\n"), "{backup}");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let facts = requests[0]["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        facts.contains("1. Likes green tea.\n2. Has a cat named Mochi.\n3. Prefers"),
        "{facts}"
    );
    assert!(requests[0]["tools"].is_null());
}
