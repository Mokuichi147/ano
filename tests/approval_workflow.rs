use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::process::Command;

/// In auto mode a reviewer model approves a read-only call on its own; a
/// call it wants the user to confirm is denied when nobody can be asked.
#[tokio::test]
async fn auto_mode_reviews_mcp_calls_and_denies_deferred_ones_without_a_terminal() {
    let agent_requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let reviews = Arc::new(Mutex::new(Vec::<Value>::new()));
    let (captured_agent, captured_reviews) = (Arc::clone(&agent_requests), Arc::clone(&reviews));
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let (agent_requests, reviews) =
                (Arc::clone(&captured_agent), Arc::clone(&captured_reviews));
            async move {
                if payload["text"]["format"]["name"] == "tool_call_review" {
                    let text = payload["input"][0]["content"][0]["text"].as_str().unwrap();
                    let verdict = if text.contains("list_issues") {
                        json!({"decision": "allow", "reason": "Reads issues the user asked about."})
                    } else {
                        json!({"decision": "ask", "reason": "Deleting is irreversible."})
                    };
                    reviews.lock().unwrap().push(payload);
                    return Json(json!({"id": "review", "status": "completed", "output": [
                        {"type": "message", "content": [{"type": "output_text", "text": verdict.to_string()}]}
                    ]}));
                }
                let mut requests = agent_requests.lock().unwrap();
                requests.push(payload);
                let output = match requests.len() {
                    1 => json!([{"type": "function_call", "call_id": "search", "name": "tool_search",
                        "arguments": "{\"query\":\"issue\"}"}]),
                    2 => json!([
                        {"type": "mcp_approval_request", "id": "approve_list", "server_label": "github",
                         "name": "list_issues", "arguments": "{}"},
                        {"type": "mcp_approval_request", "id": "approve_delete", "server_label": "github",
                         "name": "delete_issue", "arguments": "{\"number\":1}"}
                    ]),
                    _ => json!([{"type": "message", "content": [{"type": "output_text", "text": "done"}]}]),
                };
                Json(json!({"id": format!("resp_{}", requests.len()), "status": "completed", "output": output}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("config.toml"),
        "[agent]\napproval_model = 'reviewer-model'\n[[mcp_servers]]\nlabel = 'github'\nurl = 'https://mcp.example.test/mcp'\n[[mcp_servers.tool_catalog]]\nname = 'list_issues'\ndescription = 'List issues'\n[[mcp_servers.tool_catalog]]\nname = 'delete_issue'\ndescription = 'Delete an issue'\n",
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
                "auto",
                "--json",
                "Summarize the open issues",
            ])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    server.abort();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("automatic review does not allow will be denied"),
        "{stderr}"
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let approvals = result["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "mcp_approval")
        .map(|event| {
            (
                event["tool_name"].as_str().unwrap().to_string(),
                event["approved"].as_bool().unwrap(),
                event["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        approvals,
        vec![
            (
                "list_issues".to_string(),
                true,
                "auto: Reads issues the user asked about.".to_string()
            ),
            (
                "delete_issue".to_string(),
                false,
                "auto review deferred: Deleting is irreversible.".to_string()
            ),
        ]
    );

    let reviews = reviews.lock().unwrap();
    assert_eq!(reviews.len(), 2);
    assert!(reviews
        .iter()
        .all(|review| review["model"] == "reviewer-model"));
    assert!(reviews[0]["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Summarize the open issues"));
    let agent_requests = agent_requests.lock().unwrap();
    let answers = agent_requests[2]["input"].as_array().unwrap();
    assert_eq!(answers[0]["approval_request_id"], "approve_list");
    assert_eq!(answers[0]["approve"], true);
    assert_eq!(answers[1]["approval_request_id"], "approve_delete");
    assert_eq!(answers[1]["approve"], false);
}

/// `--allow-exec` offers workspace_exec; each command goes through the
/// approval mode, so an allowed command runs and its output reaches the model.
#[cfg(unix)]
#[tokio::test]
async fn exec_commands_run_in_the_workspace_after_approval() {
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
                    1 => json!([{"type": "function_call", "call_id": "search", "name": "tool_search",
                        "arguments": "{\"query\":\"shell command\"}"}]),
                    2 => json!([{"type": "function_call", "call_id": "exec", "name": "workspace_exec",
                        "arguments": "{\"command\":\"cat note.txt\"}"}]),
                    _ => json!([{"type": "message", "content": [{"type": "output_text", "text": "done"}]}]),
                };
                Json(json!({"id": format!("resp_{}", requests.len()), "status": "completed", "output": output}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "from the workspace").unwrap();
    let run = |extra: &'static [&'static str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ano"));
        command
            .current_dir(directory.path())
            .env("OPENAI_BASE_URL", &endpoint)
            .env("OPENAI_API_KEY", "test-fixture-key")
            .args(["run", "--json", "--quiet"])
            .args(extra)
            .arg("Show note.txt")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        async move {
            tokio::time::timeout(Duration::from_secs(20), command.output())
                .await
                .unwrap()
                .unwrap()
        }
    };

    let output = run(&["--allow-exec", "--approval-mode", "allow"]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result["events"].as_array().unwrap().iter().any(|event| {
        event["type"] == "local_tool_approval"
            && event["name"] == "workspace_exec"
            && event["approved"] == true
    }));
    let executed: Value = serde_json::from_str(
        requests.lock().unwrap()[2]["input"][0]["output"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(executed["stdout"], "from the workspace");
    assert_eq!(executed["success"], true);

    // Without --allow-exec the tool is never offered.
    requests.lock().unwrap().clear();
    let output = run(&["--approval-mode", "allow"]).await;
    assert!(output.status.success());
    let requests = requests.lock().unwrap();
    assert!(requests[1]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|tool| tool["name"] != "workspace_exec"));
    let blocked: Value =
        serde_json::from_str(requests[2]["input"][0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(blocked["error"], "tool_not_selected");
    server.abort();
}
