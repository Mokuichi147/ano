use ano::{RunOutcome, Session, SessionStatus};
use axum::{
    extract::{OriginalUri, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    path::Path,
    process::Output,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{process::Command, task::JoinHandle};

#[derive(Default)]
struct Script {
    responses: VecDeque<(&'static str, Value)>,
    requests: Vec<(String, Value)>,
}

struct MockApi {
    endpoint: String,
    script: Arc<Mutex<Script>>,
    server: JoinHandle<()>,
}

impl MockApi {
    async fn start(responses: Vec<(&'static str, Value)>) -> Self {
        async fn respond(
            State(script): State<Arc<Mutex<Script>>>,
            OriginalUri(uri): OriginalUri,
            Json(payload): Json<Value>,
        ) -> (StatusCode, Json<Value>) {
            let mut script = script.lock().unwrap();
            script.requests.push((uri.path().to_string(), payload));
            match script.responses.pop_front() {
                Some((path, response)) if path == uri.path() => (StatusCode::OK, Json(response)),
                _ => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":{"message":"Unexpected request in test script"}})),
                ),
            }
        }
        let script = Arc::new(Mutex::new(Script {
            responses: responses.into(),
            requests: Vec::new(),
        }));
        let app = Router::new()
            .route("/v1/responses", post(respond))
            .route("/v1/responses/compact", post(respond))
            .with_state(Arc::clone(&script));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            endpoint,
            script,
            server,
        }
    }
}

impl Drop for MockApi {
    fn drop(&mut self) {
        self.server.abort();
    }
}

const RESPONSE: &str = "/v1/responses";
const COMPACT: &str = "/v1/responses/compact";

fn response(id: &str, output: Vec<Value>) -> Value {
    json!({"id":id,"status":"completed","output":output,"usage":{"input_tokens":70,"output_tokens":30,"total_tokens":100}})
}

fn message(text: &str) -> Value {
    json!({"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":text}]})
}

fn call(id: &str, name: &str, arguments: Value) -> Value {
    json!({"type":"function_call","call_id":id,"name":name,"arguments":arguments.to_string()})
}

fn plan(revision: u64, status: &str) -> Value {
    call(
        &format!("plan{revision}"),
        "task_plan",
        json!({"expected_revision":revision,"explanation":null,
        "steps":[{"id":"inspect","description":"Inspect the file","status":status,"detail":null}]}),
    )
}

fn compact(output: &Value) -> Value {
    json!({"object":"response.compaction","id":"cmp1","output":output,"usage":{"input_tokens":160,"output_tokens":40,"total_tokens":200}})
}

fn window() -> Value {
    json!([
        {"role":"user","content":[{"type":"input_text","text":"Inspect the file"}]},
        {"type":"compaction","id":"opaque1","encrypted_content":"opaque-encrypted-state","provider_extension":{"preserve":true}}
    ])
}

fn workspace() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("agent.toml"), "[api]\nmax_retries=0\n[agent]\nmax_tool_rounds=8\ncompaction='remote'\n[environments.coding]\nworkspace='.'\nallow_writes=true\nallowed_tools=['workspace_*']\n").unwrap();
    directory
}

async fn run(directory: &Path, endpoint: &str, flags: &[&str], prompt: &str) -> Output {
    tokio::time::timeout(
        Duration::from_secs(20),
        Command::new(env!("CARGO_BIN_EXE_ano"))
            .current_dir(directory)
            .env("OPENAI_BASE_URL", endpoint)
            .env("OPENAI_API_KEY", "fixture-key")
            .args([
                "--config",
                "agent.toml",
                "run",
                "--environment",
                "coding",
                "--json",
                "--quiet",
            ])
            .args(flags)
            .arg(prompt)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap()
}

fn result(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn compacted_sessions_archive_tool_results_keep_plans_and_resume_across_processes() {
    let canonical = window();
    let api = MockApi::start(vec![
        (
            RESPONSE,
            response(
                "r1",
                vec![
                    plan(0, "in_progress"),
                    call("search", "tool_search", json!({"query":"workspace_read"})),
                ],
            ),
        ),
        (
            RESPONSE,
            response(
                "r2",
                vec![call("read", "workspace_read", json!({"path":"large.txt"}))],
            ),
        ),
        (COMPACT, compact(&canonical)),
        (RESPONSE, response("r3", vec![plan(1, "completed")])),
        (RESPONSE, response("r4", vec![message("Inspected")])),
        (RESPONSE, response("r5", vec![message("Continued")])),
    ])
    .await;
    let directory = workspace();
    std::fs::write(directory.path().join("large.txt"), "x".repeat(12_000)).unwrap();
    let flags = [
        "--session",
        ".ano/work.json",
        "--compact-threshold-bytes",
        "8192",
    ];
    let first = result(run(directory.path(), &api.endpoint, &flags, "Inspect the file").await);
    assert_eq!(first["outcome"], "completed");
    assert_eq!(first["usage"]["responses"], 4);
    assert_eq!(first["usage"]["compactions"], 1);
    assert_eq!(first["usage"]["total_tokens"], 600);
    let saved = Session::inspect(directory.path().join(".ano/work.json")).unwrap();
    assert_eq!(saved.plan.revision, 2);
    assert_eq!(saved.plan.outcome(), RunOutcome::Completed);
    assert_eq!(saved.compactions.len(), 1);
    assert_eq!(saved.history[..2], *canonical.as_array().unwrap());
    let archive = Session::inspect(
        directory
            .path()
            .join(".ano")
            .join(saved.compactions[0].archive_file.as_ref().unwrap()),
    )
    .unwrap();
    assert_eq!(archive.plan.revision, 1);
    assert_eq!(archive.plan.outcome(), RunOutcome::Incomplete);
    assert_eq!(
        serde_json::to_value(&archive).unwrap()["pending_calls"],
        json!([])
    );
    assert!(archive
        .history
        .iter()
        .any(|item| item["type"] == "function_call_output" && item["call_id"] == "read"));
    let second = result(run(directory.path(), &api.endpoint, &flags, "Continue").await);
    assert_eq!(second["usage"]["total_tokens"], 100);
    let saved = Session::inspect(directory.path().join(".ano/work.json")).unwrap();
    assert_eq!(saved.usage.total_tokens, 700);
    assert_eq!(saved.completed_turns, 2);
    let script = api.script.lock().unwrap();
    assert_eq!(script.requests.len(), 6);
    assert!(script.responses.is_empty());
    assert_eq!(script.requests[2].1["input"], json!(archive.history));
    assert_eq!(script.requests[3].1["input"], canonical);
    assert_eq!(
        &script.requests[5].1["input"].as_array().unwrap()[..2],
        canonical.as_array().unwrap()
    );
    for (path, request) in &script.requests {
        assert!(request.get("previous_response_id").is_none());
        if path == RESPONSE {
            assert_eq!(request["store"], false);
        }
    }
}

#[tokio::test]
async fn token_limit_closes_unexecuted_calls_and_resume_starts_a_fresh_budget() {
    let api = MockApi::start(vec![
        (RESPONSE, response("r1", vec![call("search", "tool_search", json!({"query":"workspace_write"}))])),
        (RESPONSE, response("r2", vec![
            call("write", "workspace_write", json!({"path":"must-not-exist.txt","content":"not authorized after the limit"})),
            json!({"type":"mcp_approval_request","id":"approve1","server_label":"remote","name":"write","arguments":"{}"})
        ])),
        (RESPONSE, response("r3", vec![message("Resumed without replay")])),
    ]).await;
    let directory = workspace();
    let flags = ["--session", ".ano/work.json", "--max-total-tokens", "200"];
    let first = result(run(directory.path(), &api.endpoint, &flags, "Write the file").await);
    assert_eq!(first["outcome"], "incomplete");
    assert_eq!(first["stop_reason"], "token_limit");
    assert_eq!(first["usage"]["total_tokens"], 200);
    assert!(!directory.path().join("must-not-exist.txt").exists());
    assert_eq!(api.script.lock().unwrap().requests.len(), 2);
    let saved = Session::inspect(directory.path().join(".ano/work.json")).unwrap();
    assert_eq!(saved.status, SessionStatus::Ready);
    assert_eq!(
        serde_json::to_value(&saved).unwrap()["pending_calls"],
        json!([])
    );
    let skipped = saved
        .history
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "write")
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(skipped["output"].as_str().unwrap()).unwrap()["error"],
        "execution_limit"
    );
    assert!(saved
        .history
        .iter()
        .any(|item| item["type"] == "mcp_approval_response"
            && item["approval_request_id"] == "approve1"
            && item["approve"] == false));
    let second = result(
        run(
            directory.path(),
            &api.endpoint,
            &flags,
            "Inspect prior results",
        )
        .await,
    );
    assert_eq!(second["outcome"], "completed");
    assert_eq!(second["usage"]["total_tokens"], 100);
    let saved = Session::inspect(directory.path().join(".ano/work.json")).unwrap();
    assert_eq!(saved.usage.total_tokens, 300);
    assert!(!directory.path().join("must-not-exist.txt").exists());
    assert_eq!(
        saved
            .history
            .iter()
            .filter(|item| item["type"] == "function_call_output" && item["call_id"] == "write")
            .count(),
        1
    );
    let script = api.script.lock().unwrap();
    assert_eq!(script.requests.len(), 3);
    assert!(script.requests[2].1["input"]
        .as_array()
        .unwrap()
        .contains(skipped));
}

#[tokio::test]
async fn missing_usage_with_a_budget_stops_before_any_local_tool() {
    let mut reply = response("r1", vec![plan(0, "in_progress")]);
    reply.as_object_mut().unwrap().remove("usage");
    let api = MockApi::start(vec![(RESPONSE, reply)]).await;
    let directory = workspace();
    let output = result(
        run(
            directory.path(),
            &api.endpoint,
            &["--max-total-tokens", "1000"],
            "Start work",
        )
        .await,
    );
    assert_eq!(output["stop_reason"], "usage_unavailable");
    assert_eq!(output["outcome"], "incomplete");
    assert_eq!(output["usage"]["unreported_requests"], 1);
    assert_eq!(output["plan"]["revision"], 0);
    assert_eq!(api.script.lock().unwrap().requests.len(), 1);
}

#[tokio::test]
async fn compaction_works_without_a_session_and_counts_toward_the_budget() {
    for limit in [200, 1000] {
        let canonical = window();
        let mut responses = vec![(COMPACT, compact(&canonical))];
        if limit > 200 {
            responses.push((RESPONSE, response("r1", vec![message("Done")])));
        }
        let api = MockApi::start(responses).await;
        let directory = workspace();
        let output = result(
            run(
                directory.path(),
                &api.endpoint,
                &[
                    "--compact-threshold-bytes",
                    "1024",
                    "--max-total-tokens",
                    &limit.to_string(),
                ],
                &"x".repeat(2000),
            )
            .await,
        );
        assert_eq!(output["usage"]["compactions"], 1);
        let script = api.script.lock().unwrap();
        if limit == 200 {
            assert_eq!(output["stop_reason"], "token_limit");
            assert_eq!(output["usage"]["responses"], 0);
            assert_eq!(script.requests.len(), 1);
        } else {
            assert_eq!(output["stop_reason"], "final_answer");
            assert_eq!(output["usage"]["total_tokens"], 300);
            assert_eq!(script.requests[1].1["input"], canonical);
            assert_eq!(script.requests[1].1["store"], false);
            assert!(script.requests[1].1.get("previous_response_id").is_none());
        }
    }
}

#[tokio::test]
async fn rejected_compaction_keeps_original_history_and_can_resume_with_it() {
    let api = MockApi::start(vec![
        (
            COMPACT,
            json!({"object":"response.compaction","id":"invalid","output":[]}),
        ),
        (RESPONSE, response("r1", vec![message("Recovered")])),
    ])
    .await;
    let directory = workspace();
    let prompt = "original input ".repeat(200);
    let output = run(
        directory.path(),
        &api.endpoint,
        &[
            "--session",
            ".ano/work.json",
            "--compact-threshold-bytes",
            "1024",
        ],
        &prompt,
    )
    .await;
    assert!(!output.status.success());
    let saved = Session::inspect(directory.path().join(".ano/work.json")).unwrap();
    assert_eq!(saved.status, SessionStatus::Failed);
    assert_eq!(saved.history[0]["content"][0]["text"], prompt);
    assert!(saved.compactions.is_empty());
    assert_eq!(
        std::fs::read_dir(directory.path().join(".ano"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("archive-"))
            .count(),
        0
    );
    let recovered = result(
        run(
            directory.path(),
            &api.endpoint,
            &["--session", ".ano/work.json"],
            "Continue without compaction",
        )
        .await,
    );
    assert_eq!(recovered["outcome"], "completed");
    let script = api.script.lock().unwrap();
    assert_eq!(script.requests.len(), 2);
    assert_eq!(
        script.requests[1].1["input"][0]["content"][0]["text"],
        prompt
    );
}

#[tokio::test]
async fn in_memory_compaction_includes_completed_tool_results() {
    let canonical = window();
    let api = MockApi::start(vec![
        (
            RESPONSE,
            response(
                "r1",
                vec![call(
                    "search",
                    "tool_search",
                    json!({"query":"workspace_read"}),
                )],
            ),
        ),
        (
            RESPONSE,
            response(
                "r2",
                vec![call("read", "workspace_read", json!({"path":"large.txt"}))],
            ),
        ),
        (COMPACT, compact(&canonical)),
        (
            RESPONSE,
            response("r3", vec![message("Inspected without a session")]),
        ),
    ])
    .await;
    let directory = workspace();
    std::fs::write(directory.path().join("large.txt"), "x".repeat(12_000)).unwrap();
    let output = result(
        run(
            directory.path(),
            &api.endpoint,
            &["--compact-threshold-bytes", "8192"],
            "Inspect the file",
        )
        .await,
    );
    assert_eq!(output["outcome"], "completed");
    assert_eq!(output["usage"]["total_tokens"], 500);
    let script = api.script.lock().unwrap();
    assert_eq!(script.requests.len(), 4);
    let history = script.requests[2].1["input"].as_array().unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .count(),
        2
    );
    let read = history
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "read")
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(read["output"].as_str().unwrap()).unwrap()["content"]
            .as_str()
            .unwrap()
            .len(),
        12_000
    );
    assert_eq!(script.requests[3].1["input"], canonical);
    assert!(script
        .requests
        .iter()
        .all(|(_, payload)| payload.get("previous_response_id").is_none()));
    assert!(!directory.path().join(".ano").exists());
}

#[tokio::test]
async fn summary_compaction_replaces_history_on_endpoints_without_the_compact_api() {
    let api = MockApi::start(vec![
        (
            RESPONSE,
            response(
                "r1",
                vec![call(
                    "search",
                    "tool_search",
                    json!({"query":"workspace_read"}),
                )],
            ),
        ),
        (
            RESPONSE,
            response(
                "r2",
                vec![call("read", "workspace_read", json!({"path":"large.txt"}))],
            ),
        ),
        (
            RESPONSE,
            response(
                "sum1",
                vec![message("large.txt を読んだ。中身は x の繰り返し。")],
            ),
        ),
        (RESPONSE, response("r3", vec![message("要約から続行")])),
    ])
    .await;
    let directory = workspace();
    std::fs::write(
        directory.path().join("agent.toml"),
        "[api]\nmax_retries=0\n[agent]\nmax_tool_rounds=8\ncompaction='summary'\n[environments.coding]\nworkspace='.'\nallowed_tools=['workspace_*']\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("large.txt"), "x".repeat(12_000)).unwrap();
    let output = result(
        run(
            directory.path(),
            &api.endpoint,
            &[
                "--session",
                ".ano/work.json",
                "--compact-threshold-bytes",
                "8192",
            ],
            "ファイルを調べて",
        )
        .await,
    );
    assert_eq!(output["text"], "要約から続行");
    assert_eq!(output["usage"]["compactions"], 1);
    let script = api.script.lock().unwrap();
    assert!(script.requests.iter().all(|(path, _)| path == RESPONSE));
    // The summary request sees a shortened transcript, not the raw history.
    let summary_request = &script.requests[2].1;
    let transcript = summary_request["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(transcript.contains("## Tool call: workspace_read"));
    assert!(transcript.contains("characters omitted"));
    assert!(summary_request["tools"].is_null());
    // The next request continues from the request and the summary.
    let history = script.requests[3].1["input"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["content"][0]["text"], "ファイルを調べて");
    assert!(history[0]["content"][1]["text"]
        .as_str()
        .unwrap()
        .ends_with("large.txt を読んだ。中身は x の繰り返し。"));
    let saved = Session::inspect(directory.path().join(".ano/work.json")).unwrap();
    assert_eq!(saved.compactions.len(), 1);
    assert!(saved.compactions[0].id.starts_with("summary-"));
    assert!(saved.compactions[0].archive_file.is_some());
}
