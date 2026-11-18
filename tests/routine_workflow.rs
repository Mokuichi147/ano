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

/// A run knows the current time and sets up a weekly routine through
/// approval; `ano routines` lists it, and the next run sees it.
#[tokio::test]
async fn runs_set_up_routines_that_later_runs_see() {
    let (endpoint, requests) = mock_responses(vec![
        json!([{"type": "function_call", "call_id": "create", "name": "routine_create",
                "arguments": json!({"kind": "task", "title": "Weekly review", "task": "Sum up my week.", "cron": "0 17 * * FRI"}).to_string()}]),
        json!([{"type": "message", "content": [{"type": "output_text", "text": "set"}]}]),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[routines]\nenabled = true\ndir = 'routines'\n",
    )
    .unwrap();
    let run = |prompt: &'static str| {
        ano(directory.path())
            .env("OPENAI_BASE_URL", &endpoint)
            .env("OPENAI_API_KEY", "test-fixture-key")
            .args(["run", "--approval-mode", "allow", "--json", prompt])
            .output()
    };
    let output = tokio::time::timeout(Duration::from_secs(20), run("毎週金曜17時に振り返りをして"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    {
        let requests = requests.lock().unwrap();
        let instructions = requests[0]["instructions"].as_str().unwrap();
        assert!(
            instructions.contains("# Routines\nThe current local time is ")
                && instructions.contains("\n\nNo routine is set up."),
            "{instructions}"
        );
    }
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["events"].as_array().unwrap().iter().any(|event| {
            event["type"] == "local_tool_approval"
                && event["name"] == "routine_create"
                && event["approved"] == true
        }),
        "{result:#}"
    );

    let listed = ano(directory.path())
        .args(["routines"])
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        stdout.contains("  [r1] task Weekly review \"0 17 * * FRI\" — next "),
        "{stdout}"
    );

    let output = tokio::time::timeout(Duration::from_secs(20), run("予定は？"))
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    let last = requests.lock().unwrap().last().unwrap().clone();
    let instructions = last["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("\n- [r1] task Weekly review at "),
        "{instructions}"
    );

    let removed = ano(directory.path())
        .args(["routines", "remove", "r1"])
        .output()
        .await
        .unwrap();
    assert!(removed.status.success());
}

/// `ano routines add` checks the time it is given.
#[tokio::test]
async fn the_command_line_refuses_times_it_cannot_read() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[routines]\ndir = 'routines'\n",
    )
    .unwrap();
    for args in [
        vec!["routines", "add", "Call mom", "--at", "tomorrow"],
        vec!["routines", "add", "Call mom", "--at", "2001-01-01 09:00"],
        vec!["routines", "add", "Call mom", "--cron", "every day"],
        vec!["routines", "add", "Call mom"],
    ] {
        let output = ano(directory.path()).args(&args).output().await.unwrap();
        assert!(!output.status.success(), "{args:?}");
    }
    let output = ano(directory.path())
        .args(["routines", "add", "Stretch", "--cron", "0 15 * * *"])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("Set up [r1] Stretch — next "));
}
