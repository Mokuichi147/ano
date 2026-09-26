use axum::{http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::process::Command;

// Invoked by the configured validation command below in a separate process.
#[test]
#[ignore = "subprocess fixture for workspace_check"]
fn check_edited_file_fixture() {
    assert_eq!(std::fs::read_to_string("note.txt").unwrap(), "done\n");
    println!("check verified persisted edit");
}

fn call(id: &str, name: &str, arguments: Value) -> Value {
    json!({"type":"function_call","call_id":id,"name":name,"arguments":arguments.to_string()})
}

async fn run_cli(
    root: &Path,
    endpoint: &str,
    prompt: &str,
    restrict: bool,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ano"));
    command
        .current_dir(root)
        .env("OPENAI_BASE_URL", endpoint)
        .env("OPENAI_API_KEY", "mock-key")
        .args([
            "--config",
            "agent.toml",
            "run",
            "--environment",
            "coding",
            "--session",
            ".ano/work.json",
            "--json",
            "--quiet",
            prompt,
        ])
        .kill_on_drop(true);
    if restrict {
        command.args(["--disable-tool", "workspace_edit"]);
    }
    tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .unwrap()
        .unwrap()
}

async fn workflow(fail_after_check: bool) {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route("/v1/responses", post(move |Json(payload): Json<Value>| {
        let captured = Arc::clone(&captured);
        async move {
            let mut requests = captured.lock().unwrap();
            requests.push(payload.clone());
            let round = requests.len();
            if fail_after_check && round == 5 {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":{"message":"fixture unavailable"}}))).into_response();
            }
            let output = match round {
                1 => vec![call("search", "tool_search", json!({"query":"workspace read edit check"}))],
                2 => vec![call("read", "workspace_read", json!({"path":"note.txt"}))],
                3 => {
                    let last = payload["input"].as_array().unwrap().last().unwrap();
                    let read: Value = serde_json::from_str(last["output"].as_str().unwrap()).unwrap();
                    vec![call("edit", "workspace_edit", json!({"path":"note.txt","expected_sha256":read["sha256"],"edits":[{"old_text":"todo","new_text":"done"}]}))]
                },
                4 => vec![call("check", "workspace_check", json!({"name":"validate"}))],
                6 if !fail_after_check => vec![call("blocked_edit", "workspace_edit", json!({"path":"note.txt","edits":[{"old_text":"done","new_text":"wrong"}]}))],
                _ => vec![json!({"type":"reasoning","id":format!("reason_{round}"),"encrypted_content":"opaque","summary":[]}),
                    json!({"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"finished"}]})],
            };
            Json(json!({"id":format!("response_{round}"),"status":"completed","output":output})).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "todo\n").unwrap();
    let executable = toml::Value::String(
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    std::fs::write(directory.path().join("agent.toml"), format!(
        "[api]\nmax_retries = 0\n[agent]\nmax_tool_rounds = 8\n[environments.coding]\nworkspace = '.'\nallow_writes = true\nallowed_tools = ['workspace_*']\n[environments.coding.checks.validate]\nprogram = {executable}\nargs = ['--exact','check_edited_file_fixture','--ignored','--nocapture']\ntimeout_secs = 5\n"
    )).unwrap();
    let first = run_cli(directory.path(), &endpoint, "read, edit, and verify", false).await;
    assert_eq!(
        first.status.success(),
        !fail_after_check,
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        "done\n"
    );
    let saved = ano::Session::inspect(directory.path().join(".ano/work.json")).unwrap();
    assert_eq!(
        saved.status,
        if fail_after_check {
            ano::SessionStatus::Failed
        } else {
            ano::SessionStatus::Ready
        }
    );
    let check = saved
        .history
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "check")
        .unwrap();
    let checked: Value = serde_json::from_str(check["output"].as_str().unwrap()).unwrap();
    assert_eq!(checked["success"], true);
    assert!(checked["stdout"]
        .as_str()
        .unwrap()
        .contains("check verified persisted edit"));

    let second = run_cli(
        directory.path(),
        &endpoint,
        "continue from previous results",
        true,
    )
    .await;
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        "done\n"
    );
    let saved = ano::Session::inspect(directory.path().join(".ano/work.json")).unwrap();
    assert_eq!(saved.status, ano::SessionStatus::Ready);
    assert_eq!(saved.completed_turns, if fail_after_check { 1 } else { 2 });
    assert_eq!(
        saved
            .history
            .iter()
            .filter(|item| item["type"] == "function_call_output" && item["call_id"] == "edit")
            .count(),
        1
    );
    if !fail_after_check {
        let result: Value = serde_json::from_slice(&second.stdout).unwrap();
        assert!(result["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["type"] == "local_tool_blocked"));
    }
    let inspection = Command::new(env!("CARGO_BIN_EXE_ano"))
        .current_dir(directory.path())
        .args([
            "--config",
            "agent.toml",
            "session",
            ".ano/work.json",
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(inspection.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&inspection.stdout).unwrap()["status"],
        "ready"
    );
    let requests = requests.lock().unwrap();
    assert!(requests
        .iter()
        .all(|payload| payload["store"] == false && payload.get("previous_response_id").is_none()));
    assert!(requests[5]["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["type"] == "function_call_output" && item["call_id"] == "check"));
    if !fail_after_check {
        assert!(requests[5]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["phase"] == "final_answer"));
    }
    server.abort();
}

#[tokio::test]
async fn sessions_continue_across_processes_and_reapply_current_policy() {
    workflow(false).await;
}

#[tokio::test]
async fn failed_turn_keeps_completed_edits_and_checks_for_the_next_request() {
    workflow(true).await;
}
