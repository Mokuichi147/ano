use super::{
    cancel_job, create_job, finish,
    jobs::{evict_finished_jobs, JobOutcome, JobRecord, JobState, JobStatus},
    router, shutdown_jobs,
    signature::{remember_signature, verify_signature, HmacSha256},
    unix_now, WebhookState,
};
use crate::{
    application::{agent::AgentResult, registry::ToolRegistry},
    config::AppConfig,
    domain::{
        plan::{RunOutcome, TaskPlan},
        usage::{StopReason, UsageSummary},
    },
    harness::Harness,
    infrastructure::{mcp::McpPool, openai::OpenAiClient},
};
use axum::{
    body::{to_bytes, Bytes},
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    routing::post,
    Json, Router,
};
use hmac::Mac;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    sync::{watch, Notify, RwLock, Semaphore},
    task::{JoinHandle, JoinSet},
};

fn completed(text: &str, plan: TaskPlan) -> JobOutcome {
    JobOutcome::Completed(Box::new(AgentResult {
        text: text.into(),
        response_id: "test_response".into(),
        events: Vec::new(),
        outcome: plan.outcome(),
        plan,
        usage: UsageSummary::default(),
        stop_reason: StopReason::FinalAnswer,
        streamed: false,
    }))
}

fn state() -> WebhookState {
    let mut config = AppConfig::default();
    config
        .environments
        .insert("default".into(), Default::default());
    WebhookState {
        harness: Harness {
            registry: ToolRegistry::new(),
            history: None,
            skills: None,
        },
        config,
        client: Arc::new(OpenAiClient::new("test", "http://127.0.0.1:1234/v1")),
        connections: Default::default(),
        mcp: Arc::new(McpPool::new(Vec::new())),
        jobs: RwLock::new(HashMap::new()),
        job_slots: Arc::new(Semaphore::new(1)),
        shutdown: watch::channel(false).0,
        tasks: Mutex::new(JoinSet::new()),
        secret: Some(b"secret".to_vec()),
        seen_signatures: Mutex::new(HashMap::new()),
    }
}

fn signed_headers(timestamp: u64, payload: &[u8]) -> HeaderMap {
    let mut mac = HmacSha256::new_from_slice(b"secret").unwrap();
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(payload);
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-ano-signature",
        HeaderValue::from_str(&signature).unwrap(),
    );
    headers.insert(
        "x-ano-timestamp",
        HeaderValue::from_str(&timestamp.to_string()).unwrap(),
    );
    headers
}

async fn enqueue(state: &Arc<WebhookState>, task: &str) -> String {
    let payload = Bytes::from(serde_json::to_vec(&json!({"task": task})).unwrap());
    let response = create_job(
        State(Arc::clone(state)),
        signed_headers(unix_now(), &payload),
        payload,
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    value["job_id"].as_str().unwrap().to_string()
}

async fn wait_for_job(state: &WebhookState, id: &str, expected: JobState) -> JobStatus {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = state.jobs.read().await[id].snapshot.clone();
            if status.status == expected {
                return status;
            }
            assert!(
                !status.status.is_finished(),
                "unexpected terminal state: {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("job did not reach expected state")
}

async fn api_fixture() -> (OpenAiClient, Arc<Notify>, JoinHandle<()>) {
    let started = Arc::new(Notify::new());
    let observed = Arc::clone(&started);
    let app = Router::new().route("/v1/responses", post(move |Json(payload): Json<Value>| {
        let observed = Arc::clone(&observed);
        async move {
            let continues_progress = payload["input"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["call_id"] == "progress_search"));
            if payload["input"][0]["content"][0]["text"] == "hang" || continues_progress {
                observed.notify_one();
                return std::future::pending::<Json<Value>>().await;
            }
            if payload["input"][0]["content"][0]["text"] == "progress" {
                let plan = json!({"expected_revision":0,"explanation":null,"steps":[
                    {"id":"inspect","description":"Inspect the repository","status":"in_progress","detail":null}]});
                return Json(json!({"id":"progress-response", "status":"completed", "output":[
                    {"type":"function_call", "call_id":"progress_plan", "name":"task_plan", "arguments":plan.to_string()},
                    {"type":"function_call", "call_id":"progress_search", "name":"tool_search",
                     "arguments":json!({"query":"x".repeat(3000)}).to_string()}
                ]}));
            }
            let output = if payload["input"][0]["content"][0]["text"] == "panic" {
                json!([{"type":"function_call", "call_id":"search", "name":"tool_search", "arguments":"{\"query\":\"panic_tool\"}"}])
            } else if payload["input"][0]["call_id"] == "search" {
                json!([{"type":"function_call", "call_id":"panic", "name":"panic_tool", "arguments":"{}"}])
            } else {
                json!([{"type":"message", "content":[{"type":"output_text", "text":"done"}]}])
            };
            Json(json!({"id":"test-response", "status":"completed", "output":output}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = OpenAiClient::new(
        "test",
        format!("http://{}/v1", listener.local_addr().unwrap()),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (client, started, server)
}

#[tokio::test]
async fn queued_cancellation_requires_its_own_signature_and_is_idempotent() {
    let state = Arc::new(state());
    let _permit = state.job_slots.acquire().await.unwrap();
    let id = enqueue(&state, "queued").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/jobs/{id}/cancel", listener.local_addr().unwrap());
    let app = router(Arc::clone(&state));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    let denied = client
        .post(&url)
        .headers(signed_headers(unix_now(), id.as_bytes()))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    assert!(!state.jobs.read().await[&id].snapshot.cancellation_requested);

    let signature = signed_headers(unix_now(), format!("cancel:{id}").as_bytes());
    let accepted = client
        .post(&url)
        .headers(signature.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    assert_eq!(
        accepted.json::<Value>().await.unwrap()["cancellation_requested"],
        true
    );
    let cancelled = wait_for_job(&state, &id, JobState::Cancelled).await;
    assert!(cancelled.started_at_unix.is_none());
    assert!(cancelled.finished_at_unix.is_some());
    let repeated = client.post(&url).headers(signature).send().await.unwrap();
    assert_eq!(repeated.status(), StatusCode::OK);
    assert_eq!(
        repeated.json::<Value>().await.unwrap()["status"],
        "cancelled"
    );
    shutdown_jobs(&state).await;
    server.abort();
}

#[tokio::test]
async fn cancelling_running_job_releases_slot_and_preserves_completed_results() {
    let (client, started, server) = api_fixture().await;
    let mut state = state();
    state.client = Arc::new(client);
    let state = Arc::new(state);
    let id = enqueue(&state, "hang").await;
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    let response = cancel_job(
        State(Arc::clone(&state)),
        signed_headers(unix_now(), format!("cancel:{id}").as_bytes()),
        Path(id.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let cancelled = wait_for_job(&state, &id, JobState::Cancelled).await;
    assert!(cancelled.started_at_unix.is_some());
    let next = enqueue(&state, "fast").await;
    let completed = wait_for_job(&state, &next, JobState::Completed).await;
    assert_eq!(completed.result.as_deref(), Some("done"));
    let response = cancel_job(
        State(Arc::clone(&state)),
        signed_headers(unix_now(), format!("cancel:{next}").as_bytes()),
        Path(next.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        state.jobs.read().await[&next].snapshot.status,
        JobState::Completed
    );
    assert!(
        !state.jobs.read().await[&next]
            .snapshot
            .cancellation_requested
    );
    shutdown_jobs(&state).await;
    server.abort();
}

#[tokio::test]
async fn running_jobs_report_plan_and_recent_events() {
    let (client, started, server) = api_fixture().await;
    let mut state = state();
    state.client = Arc::new(client);
    let state = Arc::new(state);
    let id = enqueue(&state, "progress").await;
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();

    let status = state.jobs.read().await[&id].status();
    assert_eq!(status.status, JobState::Running);
    assert_eq!(status.plan.unwrap().steps[0].id, "inspect");
    let mut kinds = status
        .recent_events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    kinds.sort();
    assert_eq!(kinds, vec!["plan_updated", "tool_search"]);
    // A long query is kept as a bounded preview.
    let search = status
        .recent_events
        .iter()
        .find(|event| event["type"] == "tool_search")
        .unwrap();
    assert_eq!(search["query"]["truncated"], true);
    shutdown_jobs(&state).await;
    server.abort();
}

#[tokio::test]
async fn job_timeout_releases_capacity_for_queued_work() {
    let (client, started, server) = api_fixture().await;
    let mut state = state();
    state.client = Arc::new(client);
    state.config.webhook.job_timeout_secs = 1;
    let state = Arc::new(state);
    let first = enqueue(&state, "hang").await;
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    let next = enqueue(&state, "fast").await;
    assert_eq!(
        state.jobs.read().await[&next].snapshot.status,
        JobState::Queued
    );
    let timed_out = wait_for_job(&state, &first, JobState::TimedOut).await;
    assert!(timed_out.error.unwrap().contains("1 seconds"));
    assert!(timed_out.started_at_unix.is_some());
    assert!(timed_out.finished_at_unix.is_some());
    assert!(!timed_out.cancellation_requested);
    wait_for_job(&state, &next, JobState::Completed).await;
    shutdown_jobs(&state).await;
    server.abort();
}

#[tokio::test]
async fn shutdown_cancels_running_and_queued_jobs_and_stops_admission() {
    let (client, started, server) = api_fixture().await;
    let mut state = state();
    state.client = Arc::new(client);
    let state = Arc::new(state);
    let running = enqueue(&state, "hang").await;
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();
    let queued = enqueue(&state, "queued").await;
    tokio::time::timeout(Duration::from_secs(2), shutdown_jobs(&state))
        .await
        .unwrap();
    assert_eq!(
        state.jobs.read().await[&running].snapshot.status,
        JobState::Cancelled
    );
    assert_eq!(
        state.jobs.read().await[&queued].snapshot.status,
        JobState::Cancelled
    );
    assert!(state.jobs.read().await[&queued]
        .snapshot
        .started_at_unix
        .is_none());
    assert!(state.tasks.lock().unwrap().is_empty());
    let payload = Bytes::from_static(b"{\"task\":\"after shutdown\"}");
    let response = create_job(
        State(Arc::clone(&state)),
        signed_headers(unix_now(), &payload),
        payload,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(state.seen_signatures.lock().unwrap().len(), 2);
    server.abort();
}

#[tokio::test]
async fn tool_panic_finishes_job_and_next_job_can_run() {
    async fn panic_tool(_: Value) -> anyhow::Result<Value> {
        panic!("test tool panic")
    }
    let (client, _, server) = api_fixture().await;
    let mut state = state();
    state.client = Arc::new(client);
    state
        .harness
        .registry
        .register(
            crate::ToolDefinition::new(
                "panic_tool",
                "A test tool",
                json!({"type":"object","properties":{},"additionalProperties":false}),
            ),
            panic_tool,
        )
        .unwrap();
    let state = Arc::new(state);
    let first = enqueue(&state, "panic").await;
    let failed = wait_for_job(&state, &first, JobState::Failed).await;
    assert_eq!(failed.error.as_deref(), Some("job execution panicked"));
    let next = enqueue(&state, "fast").await;
    wait_for_job(&state, &next, JobState::Completed).await;
    shutdown_jobs(&state).await;
    server.abort();
}

#[tokio::test]
async fn acknowledged_cancellation_wins_completion_race() {
    let state = Arc::new(state());
    let _permit = state.job_slots.acquire().await.unwrap();
    let id = enqueue(&state, "queued").await;
    let response = cancel_job(
        State(Arc::clone(&state)),
        signed_headers(unix_now(), format!("cancel:{id}").as_bytes()),
        Path(id.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    finish(&state, &id, completed("late result", TaskPlan::default())).await;
    finish(&state, &id, JobOutcome::Failed("later error".into())).await;
    let status = &state.jobs.read().await[&id].snapshot;
    assert_eq!(status.status, JobState::Cancelled);
    assert!(status.result.is_none());
}

#[tokio::test]
async fn retention_is_enforced_at_completion_without_evicting_pending_jobs() {
    let mut state = state();
    state.config.webhook.max_retained_jobs = 1;
    let state = Arc::new(state);
    let _permit = state.job_slots.acquire().await.unwrap();
    let old = enqueue(&state, "first").await;
    let new = enqueue(&state, "second").await;
    let pending = enqueue(&state, "third").await;
    finish(&state, &old, completed("old", TaskPlan::default())).await;
    assert_eq!(state.jobs.read().await.len(), 3);
    finish(&state, &new, completed("new", TaskPlan::default())).await;
    let jobs = state.jobs.read().await;
    assert_eq!(jobs.len(), 2);
    assert!(!jobs.contains_key(&old));
    assert_eq!(jobs[&new].snapshot.result.as_deref(), Some("new"));
    assert_eq!(jobs[&pending].snapshot.status, JobState::Queued);
}

#[tokio::test]
async fn unfinished_plans_are_terminal_jobs_with_their_remaining_steps() {
    let mut state = state();
    state.job_slots = Arc::new(Semaphore::new(0));
    let state = Arc::new(state);
    for (step_status, expected) in [
        ("pending", JobState::Incomplete),
        ("blocked", JobState::Blocked),
    ] {
        let id = enqueue(&state, &format!("needs more work: {step_status}")).await;
        let plan: TaskPlan = serde_json::from_value(json!({"revision":1,"explanation":null,
            "steps":[{"id":"verify","description":"Run tests","status":step_status,"detail":"Test database unavailable"}]})).unwrap();
        finish(&state, &id, completed("Still needs verification", plan)).await;
        let snapshot = state.jobs.read().await[&id].snapshot.clone();
        assert_eq!(snapshot.status, expected);
        assert!(snapshot.status.is_finished());
        assert!(snapshot.finished_at_unix.is_some());
        let json = serde_json::to_value(snapshot).unwrap();
        assert_eq!(json["plan"]["steps"][0]["status"], step_status);
        let signature = signed_headers(unix_now(), format!("cancel:{id}").as_bytes());
        let response = cancel_job(State(Arc::clone(&state)), signature, Path(id.clone())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(state.jobs.read().await[&id].snapshot.status, expected);
    }
    shutdown_jobs(&state).await;
}

#[tokio::test]
async fn token_limit_is_incomplete_even_without_pending_plan_steps() {
    let mut state = state();
    state.job_slots = Arc::new(Semaphore::new(0));
    let state = Arc::new(state);
    let id = enqueue(&state, "limited work").await;
    let JobOutcome::Completed(mut result) = completed("Budget exhausted", TaskPlan::default())
    else {
        unreachable!()
    };
    result.outcome = RunOutcome::Incomplete;
    result.stop_reason = StopReason::TokenLimit;
    result.usage = UsageSummary {
        responses: 1,
        input_tokens: 90,
        output_tokens: 10,
        total_tokens: 100,
        ..Default::default()
    };
    finish(&state, &id, JobOutcome::Completed(result)).await;
    let snapshot = state.jobs.read().await[&id].snapshot.clone();
    assert_eq!(snapshot.status, JobState::Incomplete);
    assert_eq!(snapshot.stop_reason, Some(StopReason::TokenLimit));
    assert_eq!(snapshot.usage.unwrap().total_tokens, 100);
    shutdown_jobs(&state).await;
}

#[test]
fn verifies_timestamped_hmac_signature() {
    let state = state();
    let now = unix_now();
    let payload = b"{\"task\":\"hello\"}";
    let headers = signed_headers(now, payload);

    assert!(verify_signature(&state, &headers, payload, now).is_ok());
    assert!(verify_signature(&state, &headers, b"tampered", now).is_err());
    assert!(verify_signature(&state, &HeaderMap::new(), payload, now).is_err());
}

#[test]
fn rejects_stale_signatures() {
    let state = state();
    let now = unix_now();
    let payload = b"{}";
    let headers = signed_headers(now - 3600, payload);

    assert_eq!(
        verify_signature(&state, &headers, payload, now),
        Err("expired webhook signature")
    );
}

#[test]
fn rejects_replayed_signatures() {
    let state = state();
    let now = unix_now();
    let payload = b"{}";
    let signature = verify_signature(&state, &signed_headers(now, payload), payload, now)
        .unwrap()
        .unwrap();

    assert!(remember_signature(&state, signature.clone()));
    assert!(!remember_signature(&state, signature));
}

#[tokio::test]
async fn overload_does_not_consume_request_signature() {
    let mut state = state();
    state.config.webhook.max_pending_jobs = 1;
    let state = Arc::new(state);
    // Keep accepted work queued so the test never contacts the API.
    let _permit = state.job_slots.acquire().await.unwrap();
    let payload = Bytes::from_static(b"{\"task\":\"hello\"}");
    let headers = signed_headers(unix_now(), &payload);
    state.jobs.write().await.insert(
        "existing".into(),
        JobRecord {
            snapshot: JobStatus {
                id: "existing".into(),
                status: JobState::Queued,
                user: "default".into(),
                environment: "default".into(),
                created_at_unix: unix_now(),
                started_at_unix: None,
                finished_at_unix: None,
                cancellation_requested: false,
                result: None,
                plan: None,
                usage: None,
                stop_reason: None,
                error: None,
                recent_events: Vec::new(),
            },
            cancel: watch::channel(false).0,
            finished_at: None,
            progress: Default::default(),
        },
    );

    let response = create_job(State(Arc::clone(&state)), headers.clone(), payload.clone()).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(state.seen_signatures.lock().unwrap().is_empty());

    state.jobs.write().await.remove("existing");
    let response = create_job(State(Arc::clone(&state)), headers.clone(), payload.clone()).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(state.jobs.read().await.len(), 1);
    assert_eq!(state.seen_signatures.lock().unwrap().len(), 1);
    let replay = create_job(State(Arc::clone(&state)), headers, payload).await;
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn concurrent_signed_retries_enqueue_only_one_job() {
    let state = Arc::new(state());
    let _permit = state.job_slots.acquire().await.unwrap();
    let payload = Bytes::from_static(b"{\"task\":\"hello\"}");
    let headers = signed_headers(unix_now(), &payload);

    let (first, second) = tokio::join!(
        create_job(State(Arc::clone(&state)), headers.clone(), payload.clone()),
        create_job(State(Arc::clone(&state)), headers, payload),
    );
    let mut statuses = [first.status(), second.status()];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::ACCEPTED, StatusCode::UNAUTHORIZED]);
    assert_eq!(state.jobs.read().await.len(), 1);
}

#[test]
fn evicts_oldest_finished_jobs_only() {
    let job = |id: &str, status: JobState, finished: Option<u64>| JobRecord {
        snapshot: JobStatus {
            id: id.into(),
            status,
            user: "default".into(),
            environment: "default".into(),
            created_at_unix: 0,
            started_at_unix: None,
            finished_at_unix: finished,
            cancellation_requested: false,
            result: None,
            plan: None,
            usage: None,
            stop_reason: None,
            error: None,
            recent_events: Vec::new(),
        },
        cancel: watch::channel(false).0,
        progress: Default::default(),
        finished_at: finished.map(|_| Instant::now()),
    };
    let mut jobs = HashMap::new();
    for job in [
        job("old", JobState::Completed, Some(1)),
        job("new", JobState::Failed, Some(2)),
        job("running", JobState::Running, None),
    ] {
        jobs.insert(job.snapshot.id.clone(), job);
    }

    evict_finished_jobs(&mut jobs, 2);
    assert!(jobs.contains_key("old"));
    assert_eq!(jobs.len(), 3);
    evict_finished_jobs(&mut jobs, 1);
    assert!(!jobs.contains_key("old"));
    assert!(jobs.contains_key("new"));
    assert!(jobs.contains_key("running"));
    evict_finished_jobs(&mut jobs, 0);
    assert_eq!(jobs.len(), 1);
    assert!(jobs.contains_key("running"));
}

#[tokio::test]
async fn goals_are_validated_before_a_job_is_accepted() {
    let state = Arc::new(state());
    // Keep accepted work queued so the test never contacts the API.
    let _permit = state.job_slots.acquire().await.unwrap();
    for (goal, expected) in [
        (json!(" "), StatusCode::BAD_REQUEST),
        (json!({"objective": "Tests pass"}), StatusCode::BAD_REQUEST),
        (json!("cargo test がすべて通る"), StatusCode::ACCEPTED),
    ] {
        let payload =
            Bytes::from(serde_json::to_vec(&json!({"task": "fix", "goal": goal})).unwrap());
        let response = create_job(
            State(Arc::clone(&state)),
            signed_headers(unix_now(), &payload),
            payload,
        )
        .await;
        assert_eq!(response.status(), expected, "{goal}");
    }
}

#[tokio::test]
async fn environments_run_on_their_own_provider_and_model() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let recorded = Arc::clone(&recorded);
            async move {
                recorded.lock().unwrap().push(payload["model"].clone());
                Json(json!({"id":"r", "status":"completed", "output":[
                    {"type":"message", "content":[{"type":"output_text", "text":"done"}]}]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = OpenAiClient::new("", format!("http://{}/v1", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut state = state();
    state.config = AppConfig::parse(
        "[providers.local]\nbase_url = 'http://127.0.0.1:9/v1'\nmodel = 'qwen'\n[environments.default]\nprovider = 'local'",
    )
    .unwrap();
    // The [api] client points at a closed port, so only the provider answers.
    state.connections.insert("local", Arc::new(local));
    let state = Arc::new(state);
    let id = enqueue(&state, "hello").await;
    wait_for_job(&state, &id, JobState::Completed).await;
    assert_eq!(*seen.lock().unwrap(), vec![json!("qwen")]);
    server.abort();
}

#[tokio::test]
async fn a_broken_skill_file_does_not_fail_the_job() {
    let skills = tempfile::tempdir().unwrap();
    let broken = skills.path().join("default").join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("SKILL.md"), "no frontmatter").unwrap();
    let (client, _, server) = api_fixture().await;
    let mut state = state();
    state.client = Arc::new(client);
    let library = Arc::new(crate::infrastructure::skills::SkillLibrary::new(
        skills.path(),
    ));
    library.register_tools(&state.harness.registry).unwrap();
    state.harness.skills = Some(library);
    let state = Arc::new(state);
    let job = enqueue(&state, "fast").await;
    wait_for_job(&state, &job, JobState::Completed).await;
    shutdown_jobs(&state).await;
    server.abort();
}
