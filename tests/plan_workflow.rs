use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::process::Command;

fn plan_call(id: &str, revision: u64, status: &str) -> Value {
    json!({"type":"function_call","call_id":id,"name":"task_plan","arguments":json!({
        "expected_revision":revision,"explanation":null,
        "steps":[{"id":"verify","description":"Verify the change","status":status,"detail":null}]
    }).to_string()})
}

fn final_message(text: &str) -> Value {
    json!({"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":text}]})
}

#[tokio::test]
async fn unfinished_plans_continue_within_budget_and_resume_across_cli_processes() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route("/v1/responses", post(move |Json(payload): Json<Value>| {
        let captured = Arc::clone(&captured);
        async move {
            let mut requests = captured.lock().unwrap();
            requests.push(payload);
            let round = requests.len();
            let item = match round {
                1 => plan_call("plan1", 0, "in_progress"),
                2 => final_message("Premature final answer"),
                3 => final_message("Verification is still pending"),
                4 => json!({"type":"function_call","call_id":"read_plan","name":"task_plan","arguments":"{\"steps\":null,\"expected_revision\":null,\"explanation\":null}"}),
                5 => plan_call("plan2", 1, "completed"),
                _ => final_message("Verification complete"),
            };
            Json(json!({"id":format!("response_{round}"),"status":"completed","output":[item]}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("agent.toml"),
        "[agent]\nmax_tool_rounds=3\n",
    )
    .unwrap();
    let run = |prompt: &'static str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ano"));
        command
            .current_dir(directory.path())
            .env("OPENAI_BASE_URL", &endpoint)
            .env("OPENAI_API_KEY", "fixture-key")
            .args([
                "--config",
                "agent.toml",
                "run",
                "--session",
                ".ano/work.json",
                "--json",
                "--quiet",
                prompt,
            ])
            .kill_on_drop(true);
        async move {
            let output = tokio::time::timeout(Duration::from_secs(15), command.output())
                .await
                .unwrap()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        }
    };
    let first = run("Verify the change").await;
    assert_eq!(first["text"], "Verification is still pending");
    assert_eq!(first["outcome"], "incomplete");
    assert_eq!(first["plan"]["revision"], 1);
    assert!(first["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["type"] == "plan_updated"));
    let saved = ano::Session::inspect(directory.path().join(".ano/work.json")).unwrap();
    assert_eq!(saved.plan.outcome(), ano::RunOutcome::Incomplete);
    let second = run("Continue verification").await;
    assert_eq!(second["outcome"], "completed");
    assert_eq!(second["plan"]["revision"], 2);
    let saved = ano::Session::inspect(directory.path().join(".ano/work.json")).unwrap();
    assert_eq!(saved.plan.outcome(), ano::RunOutcome::Completed);
    assert_eq!(saved.completed_turns, 2);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    assert!(requests[2]["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("Runtime notice:"))));
    assert_eq!(requests[2]["tools"], json!([]));
    let read_plan = requests[4]["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "read_plan")
        .unwrap();
    let read_plan: Value = serde_json::from_str(read_plan["output"].as_str().unwrap()).unwrap();
    assert_eq!(read_plan["plan"]["revision"], 1);
    assert_eq!(read_plan["plan"]["steps"][0]["status"], "in_progress");
    server.abort();
}

fn goal_call(revision: u64) -> Value {
    json!({"type":"function_call","call_id":"goal","name":"task_plan","arguments":json!({
        "expected_revision":revision,"explanation":null,"steps":null,
        "goal":{"objective":"Docs are updated","acceptance":[
            {"id":"c1","description":"README mentions goals","status":"met","evidence":"README.md line 12"}]}
    }).to_string()})
}

/// A goal from the command line is saved with the session, reported with the
/// answer, and needs no prompt.
#[tokio::test]
async fn run_with_a_goal_reports_verified_criteria() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let captured = Arc::clone(&captured);
            async move {
                let mut requests = captured.lock().unwrap();
                requests.push(payload);
                let round = requests.len();
                let item = match round {
                    1 => goal_call(1),
                    _ => final_message("README に追記しました"),
                };
                Json(json!({"id":format!("response_{round}"),"status":"completed","output":[item]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let ano = |arguments: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ano"));
        command
            .current_dir(directory.path())
            .env("OPENAI_BASE_URL", &endpoint)
            .env("OPENAI_API_KEY", "fixture-key")
            .args(arguments)
            // Without a prompt, a piped stdin would be read as one.
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        async move {
            tokio::time::timeout(Duration::from_secs(15), command.output())
                .await
                .unwrap()
                .unwrap()
        }
    };

    let output = ano(&[
        "run",
        "--session",
        ".ano/goal.json",
        "--goal",
        "Docs are updated",
    ])
    .await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("README に追記しました"), "{stdout}");
    assert!(
        stdout.contains("Goal (set by the user): Docs are updated"),
        "{stdout}"
    );
    assert!(
        stdout.contains("[met] c1: README mentions goals — README.md line 12"),
        "{stdout}"
    );

    let inspected = ano(&["session", ".ano/goal.json"]).await;
    let inspected = String::from_utf8_lossy(&inspected.stdout);
    assert!(inspected.contains("Plan: Completed"), "{inspected}");
    assert!(inspected.contains("[met] c1"), "{inspected}");
    server.abort();

    let requests = requests.lock().unwrap();
    let input = requests[0]["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["content"].as_array().unwrap().len(), 1);
    assert!(input[0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Objective: Docs are updated\n"));
}
