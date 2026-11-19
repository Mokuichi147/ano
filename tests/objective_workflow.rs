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

/// An objective added from the command line is listed to the next run; the
/// model records progress and sets the next step, and `ano objectives`
/// shows the result.
#[tokio::test]
async fn runs_see_objectives_and_track_progress() {
    let (endpoint, requests) = mock_responses(vec![
        json!([
            {"type": "function_call", "call_id": "log", "name": "objective_log",
             "arguments": json!({"id": "o1", "note": "Saved 30000 yen", "value": 30000}).to_string()},
            {"type": "function_call", "call_id": "set", "name": "objective_set",
             "arguments": json!({"id": "o1", "next_step": "Review the phone plan"}).to_string()}
        ]),
        json!([{"type": "message", "content": [{"type": "output_text", "text": "nice"}]}]),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[objectives]\nenabled = true\ndir = 'objectives'\n",
    )
    .unwrap();
    let added = ano(directory.path())
        .args([
            "objectives",
            "add",
            "Save money",
            "--category",
            "money",
            "--target",
            "100000",
            "--unit",
            "yen",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

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
                "今月は3万円貯めた",
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
        instructions.contains("# Objectives\n")
            && instructions.contains("\n- [o1] Save money (money); target 100000 yen"),
        "{instructions}"
    );
    let tools = requests[0]["tools"].as_array().unwrap();
    for tool in ["objective_set", "objective_log"] {
        let offered = tools
            .iter()
            .find(|offered| offered["name"] == tool)
            .unwrap_or_else(|| panic!("{tool} is not offered"));
        // Optional fields cannot be expressed in a strict schema.
        assert_eq!(offered["strict"], false, "{offered}");
    }
    // Reading is rare and loaded through tool_search, to keep requests small.
    assert!(!tools
        .iter()
        .any(|offered| offered["name"] == "objective_read"));
    assert!(instructions.contains("objective_read"), "{instructions}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["events"].as_array().unwrap().iter().any(|event| {
            event["type"] == "local_tool_approval"
                && event["name"] == "objective_set"
                && event["approved"] == true
        }),
        "{result:#}"
    );

    let listed = ano(directory.path())
        .args(["objectives"])
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        stdout.contains(
            "active  [o1] Save money (money); 30000 / 100000 yen; next: Review the phone plan; last progress: Saved 30000 yen"
        ),
        "{stdout}"
    );
    let shown = ano(directory.path())
        .args(["objectives", "show", "o1"])
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&shown.stdout);
    assert!(stdout.contains("  Saved 30000 yen (30000)"), "{stdout}");

    let done = ano(directory.path())
        .args(["objectives", "set", "o1", "--status", "done"])
        .output()
        .await
        .unwrap();
    assert!(done.status.success());
    let listed = ano(directory.path())
        .args(["objectives"])
        .output()
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).contains("(none)"));
}

/// Without `[objectives] enabled`, no objective reaches the model.
#[tokio::test]
async fn disabled_objectives_stay_out_of_runs() {
    let (endpoint, requests) = mock_responses(vec![json!([
        {"type": "message", "content": [{"type": "output_text", "text": "hi"}]}
    ])])
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[objectives]\ndir = 'objectives'\n",
    )
    .unwrap();
    let added = ano(directory.path())
        .args(["objectives", "add", "Save money"])
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
    assert!(!instructions.contains("# Objectives"), "{instructions}");
    assert!(!requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"].as_str().unwrap().starts_with("objective_")));
}
