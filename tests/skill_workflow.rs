use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};

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

fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
    let directory = root.join("skills/default").join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
    )
    .unwrap();
}

fn tool_names(request: &Value) -> Vec<&str> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect()
}

/// A run sees the saved skills in its instructions, reads one and saves an
/// improved version without searching for the tools first, and the save goes
/// through approval.
#[tokio::test]
async fn runs_read_listed_skills_and_save_approved_improvements() {
    let (endpoint, requests) = mock_responses(vec![
        json!([{"type": "function_call", "call_id": "read", "name": "skill_read",
                "arguments": "{\"name\":\"release-build\"}"}]),
        json!([{"type": "function_call", "call_id": "save", "name": "skill_save",
                "arguments": json!({"name": "release-build", "description": "Build and verify a release.",
                                    "body": "1. cargo build --release\n2. Run the smoke test."}).to_string()}]),
        json!([{"type": "message", "content": [{"type": "output_text", "text": "done"}]}]),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    write_skill(
        directory.path(),
        "release-build",
        "Build a release.",
        "1. cargo build --release",
    );
    std::fs::write(
        directory.path().join("config.toml"),
        "[skills]\nenabled = true\ndir = 'skills'\n",
    )
    .unwrap();

    let output = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new(env!("CARGO_BIN_EXE_ano"))
            .current_dir(directory.path())
            .env("OPENAI_BASE_URL", &endpoint)
            .env("OPENAI_API_KEY", "test-fixture-key")
            .args([
                "run",
                "--approval-mode",
                "allow",
                "--json",
                "Build the release",
            ])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");

    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    let instructions = requests[0]["instructions"].as_str().unwrap();
    assert!(
        instructions.contains("# Skills")
            && instructions.contains("- release-build: Build a release."),
        "{instructions}"
    );
    for name in ["skill_read", "skill_save"] {
        assert!(tool_names(&requests[0]).contains(&name), "{name}");
    }
    let read = requests[1]["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["call_id"] == "read" && item["type"] == "function_call_output")
        .unwrap();
    assert!(
        read["output"]
            .as_str()
            .unwrap()
            .contains("cargo build --release"),
        "{read}"
    );

    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["events"].as_array().unwrap().iter().any(|event| {
            event["type"] == "local_tool_approval"
                && event["name"] == "skill_save"
                && event["approved"] == true
        }),
        "{result:#}"
    );
    let saved = std::fs::read_to_string(
        directory
            .path()
            .join("skills/default/release-build/SKILL.md"),
    )
    .unwrap();
    assert!(
        saved.contains("description: Build and verify a release.")
            && saved.contains("2. Run the smoke test."),
        "{saved}"
    );

    let listed = Command::new(env!("CARGO_BIN_EXE_ano"))
        .current_dir(directory.path())
        .arg("skills")
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        stdout.contains("release-build - Build and verify a release."),
        "{stdout}"
    );
}

/// `/skill` in a chat asks the model to save what worked, and the line as
/// typed stays the user's input in the conversation's source.
#[tokio::test]
async fn chat_skill_command_asks_the_model_to_save_what_worked() {
    let (endpoint, requests) = mock_responses(vec![json!([
        {"type": "message", "content": [{"type": "output_text", "text": "nothing to save"}]}
    ])])
    .await;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[skills]\nenabled = true\ndir = 'skills'\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ano"))
        .current_dir(directory.path())
        .env("OPENAI_BASE_URL", &endpoint)
        .env("OPENAI_API_KEY", "test-fixture-key")
        .arg("chat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all("/skill リリース手順\n".as_bytes())
        .await
        .unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let input = requests[0]["input"].to_string();
    assert!(
        input.contains("save the approach that worked as a skill with skill_save")
            && input.contains("リリース手順")
            && !input.contains("/skill"),
        "{input}"
    );
    assert!(
        requests[0]["instructions"]
            .as_str()
            .unwrap()
            .contains("No skills are saved yet."),
        "{}",
        requests[0]["instructions"]
    );
}
