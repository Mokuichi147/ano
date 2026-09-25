use crate::{
    agent::{Agent, AlwaysApprove, DenyApproval, RunRequest},
    client::OpenAiClient,
    config::AppConfig,
    input::InputPart,
    mcp::McpPool,
    tools::{ToolContext, ToolRegistry},
    AgentResult, ApprovalHandler, RunOutcome, TaskPlan,
};
use anyhow::{bail, Context, Result};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::FutureExt;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;
use std::{
    collections::HashMap,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpListener,
    sync::{watch, RwLock, Semaphore},
    task::JoinSet,
};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const SIGNATURE_HEADER: &str = "x-ano-signature";
const TIMESTAMP_HEADER: &str = "x-ano-timestamp";
const MAX_TASK_BYTES: usize = 100_000;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookTaskRequest {
    task: String,
    #[serde(default = "default_user")]
    user: String,
    #[serde(default = "default_environment")]
    environment: String,
    #[serde(default)]
    images: Vec<WebhookImage>,
    #[serde(default)]
    audio: Vec<WebhookAudio>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookImage {
    data: String,
    mime_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookAudio {
    data: String,
    format: String,
}

fn default_user() -> String {
    "default".to_string()
}

fn default_environment() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Completed,
    Blocked,
    Incomplete,
    Failed,
    Cancelled,
    TimedOut,
}

impl JobState {
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Blocked
                | Self::Incomplete
                | Self::Failed
                | Self::Cancelled
                | Self::TimedOut
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub id: String,
    pub status: JobState,
    pub user: String,
    pub environment: String,
    pub created_at_unix: u64,
    pub started_at_unix: Option<u64>,
    pub finished_at_unix: Option<u64>,
    pub cancellation_requested: bool,
    pub result: Option<String>,
    pub plan: Option<TaskPlan>,
    pub error: Option<String>,
}

struct JobRecord {
    snapshot: JobStatus,
    cancel: watch::Sender<bool>,
    /// Monotonic completion order, including jobs finished in the same second.
    finished_at: Option<Instant>,
}

enum JobOutcome {
    Completed(String, TaskPlan),
    Failed(String),
    Cancelled,
    TimedOut,
}

struct WebhookState {
    config: AppConfig,
    client: OpenAiClient,
    registry: ToolRegistry,
    /// MCP connections shared by every job.
    mcp: Arc<McpPool>,
    jobs: RwLock<HashMap<String, JobRecord>>,
    job_slots: Arc<Semaphore>,
    shutdown: watch::Sender<bool>,
    tasks: Mutex<JoinSet<()>>,
    secret: Option<Vec<u8>>,
    /// Signatures of accepted task requests -> expiry, to reject replays.
    seen_signatures: Mutex<HashMap<Vec<u8>, u64>>,
}

/// Start the inbound webhook server.
///
/// The server accepts a signed JSON task, queues it, and runs the agent in a
/// named environment from configuration. It intentionally does not allow the
/// caller to submit a raw filesystem path or tool allowlist.
pub async fn serve(
    mut config: AppConfig,
    client: OpenAiClient,
    registry: ToolRegistry,
    bind_override: Option<String>,
    path_override: Option<String>,
    allow_unauthenticated_override: bool,
) -> Result<()> {
    if let Some(bind) = bind_override {
        config.webhook.bind = bind;
    }
    if let Some(path) = path_override {
        config.webhook.path = path;
    }
    if allow_unauthenticated_override {
        config.webhook.allow_unauthenticated = true;
    }
    config.validate()?;
    let webhook = config.webhook.clone();

    let secret = match std::env::var(&webhook.secret_env) {
        Ok(secret) if secret.is_empty() => bail!("{} is set but empty", webhook.secret_env),
        Ok(secret) => Some(secret.into_bytes()),
        Err(_) => None,
    };

    let listener = TcpListener::bind(&webhook.bind)
        .await
        .with_context(|| format!("failed to bind webhook server to {}", webhook.bind))?;
    let local_addr = listener.local_addr()?;
    if secret.is_none() {
        if !webhook.allow_unauthenticated {
            bail!(
                "{} is not set; configure a webhook secret or pass --allow-unauthenticated",
                webhook.secret_env
            );
        }
        if !local_addr.ip().is_loopback() {
            bail!(
                "refusing to serve without {} on non-loopback address {local_addr}",
                webhook.secret_env
            );
        }
        eprintln!("warning: webhook authentication is disabled (loopback only)");
    }

    let mcp = Arc::new(McpPool::new(config.mcp_servers.clone()));
    let state = Arc::new(WebhookState {
        job_slots: Arc::new(Semaphore::new(webhook.max_concurrent_jobs)),
        mcp: Arc::clone(&mcp),
        config,
        client,
        registry,
        jobs: RwLock::new(HashMap::new()),
        shutdown: watch::channel(false).0,
        tasks: Mutex::new(JoinSet::new()),
        secret,
        seen_signatures: Mutex::new(HashMap::new()),
    });
    let app = router(Arc::clone(&state));

    println!(
        "ano webhook listening on http://{local_addr}{}",
        webhook.path
    );
    let shutdown_state = Arc::clone(&state);
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            signal_shutdown(&shutdown_state).await;
        })
        .await
        .context("webhook server stopped unexpectedly");
    shutdown_jobs(&state).await;
    mcp.shutdown().await;
    served
}

fn router(state: Arc<WebhookState>) -> Router {
    Router::new()
        .route(&state.config.webhook.path, post(create_job))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/cancel", post(cancel_job))
        .route("/healthz", get(healthz))
        .layer(DefaultBodyLimit::max(state.config.webhook.max_body_bytes))
        .with_state(state)
}

async fn create_job(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let signature = match verify_signature(&state, &headers, &body, unix_now()) {
        Ok(signature) => signature,
        Err(message) => return error_response(StatusCode::UNAUTHORIZED, message),
    };

    let request: WebhookTaskRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("invalid JSON: {error}"))
        }
    };
    if request.task.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "task must not be empty");
    }
    if request.task.len() > MAX_TASK_BYTES {
        return error_response(StatusCode::BAD_REQUEST, "task is too long");
    }
    if !state.config.has_user(&request.user) {
        return error_response(StatusCode::BAD_REQUEST, "unknown user");
    }
    if let Err(error) = state.config.environment_for(&request.environment) {
        return error_response(StatusCode::BAD_REQUEST, &error.to_string());
    }

    let id = Uuid::new_v4().to_string();
    {
        let mut jobs = state.jobs.write().await;
        if *state.shutdown.borrow() {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "webhook server is shutting down",
            );
        }
        // An already accepted request remains a replay even when capacity is
        // exhausted. Inspect without consuming a new signature on overload.
        if signature.as_ref().is_some_and(|signature| {
            state
                .seen_signatures
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(signature)
                .is_some_and(|expiry| *expiry > unix_now())
        }) {
            return error_response(StatusCode::UNAUTHORIZED, "replayed webhook request");
        }
        let pending = jobs
            .values()
            .filter(|job| !job.snapshot.status.is_finished())
            .count();
        if pending >= state.config.webhook.max_pending_jobs {
            return error_response(StatusCode::SERVICE_UNAVAILABLE, "too many pending jobs");
        }
        // Consume the signature only when the request can be accepted. Keep
        // this check and insertion under the jobs lock so concurrent retries
        // cannot both enqueue work, while a 503 remains safe to retry.
        if let Some(signature) = signature {
            if !remember_signature(&state, signature) {
                return error_response(StatusCode::UNAUTHORIZED, "replayed webhook request");
            }
        }
        evict_finished_jobs(&mut jobs, state.config.webhook.max_retained_jobs);
        let (cancel, cancellation) = watch::channel(false);
        jobs.insert(
            id.clone(),
            JobRecord {
                snapshot: JobStatus {
                    id: id.clone(),
                    status: JobState::Queued,
                    user: request.user.clone(),
                    environment: request.environment.clone(),
                    created_at_unix: unix_now(),
                    started_at_unix: None,
                    finished_at_unix: None,
                    cancellation_requested: false,
                    result: None,
                    plan: None,
                    error: None,
                },
                cancel,
                finished_at: None,
            },
        );
        // Register the task before releasing the admission lock. Shutdown
        // takes the same lock, so no accepted task escapes supervision.
        let task_state = Arc::clone(&state);
        let task_id = id.clone();
        let mut tasks = state
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while tasks.try_join_next().is_some() {}
        tasks.spawn(async move {
            let run = run_job(
                Arc::clone(&task_state),
                task_id.clone(),
                request,
                cancellation,
            );
            if AssertUnwindSafe(run).catch_unwind().await.is_err() {
                finish(
                    &task_state,
                    &task_id,
                    JobOutcome::Failed("job execution panicked".into()),
                )
                .await;
            }
        });
    }

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "job_id": id,
            "status": JobState::Queued,
            "status_url": format!("/jobs/{id}"),
            "cancel_url": format!("/jobs/{id}/cancel"),
        })),
    )
        .into_response()
}

async fn get_job(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(message) = verify_signature(&state, &headers, id.as_bytes(), unix_now()) {
        return error_response(StatusCode::UNAUTHORIZED, message);
    }
    match state
        .jobs
        .read()
        .await
        .get(&id)
        .map(|job| job.snapshot.clone())
    {
        Some(status) => (StatusCode::OK, Json(status)).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "job not found"),
    }
}

async fn cancel_job(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    // Domain-separate cancellation from read-only status signatures.
    let payload = format!("cancel:{id}");
    if let Err(message) = verify_signature(&state, &headers, payload.as_bytes(), unix_now()) {
        return error_response(StatusCode::UNAUTHORIZED, message);
    }
    let mut jobs = state.jobs.write().await;
    let Some(job) = jobs.get_mut(&id) else {
        return error_response(StatusCode::NOT_FOUND, "job not found");
    };
    if job.snapshot.status.is_finished() {
        return (StatusCode::OK, Json(job.snapshot.clone())).into_response();
    }
    job.snapshot.cancellation_requested = true;
    job.cancel.send_replace(true);
    (StatusCode::ACCEPTED, Json(job.snapshot.clone())).into_response()
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

async fn cancellation_requested(mut signal: watch::Receiver<bool>) {
    let _ = signal.wait_for(|cancelled| *cancelled).await;
}

async fn run_job(
    state: Arc<WebhookState>,
    id: String,
    request: WebhookTaskRequest,
    cancellation: watch::Receiver<bool>,
) {
    let shutdown = state.shutdown.subscribe();
    // Wait for a free slot while the job stays `queued`.
    let permit = tokio::select! {
        biased;
        _ = cancellation_requested(cancellation.clone()) => None,
        _ = cancellation_requested(shutdown.clone()) => None,
        permit = Arc::clone(&state.job_slots).acquire_owned() => permit.ok(),
    };
    let Some(_permit) = permit else {
        finish(&state, &id, JobOutcome::Cancelled).await;
        return;
    };
    {
        let mut jobs = state.jobs.write().await;
        let Some(job) = jobs.get_mut(&id) else { return };
        if !job.snapshot.cancellation_requested && !*state.shutdown.borrow() {
            job.snapshot.status = JobState::Running;
            job.snapshot.started_at_unix = Some(unix_now());
        }
    }
    let outcome = tokio::select! {
        biased;
        _ = cancellation_requested(cancellation) => JobOutcome::Cancelled,
        _ = cancellation_requested(shutdown) => JobOutcome::Cancelled,
        outcome = tokio::time::timeout(
            Duration::from_secs(state.config.webhook.job_timeout_secs),
            execute_job(&state, request),
        ) => match outcome {
            Ok(Ok(result)) => JobOutcome::Completed(result.text, result.plan),
            Ok(Err(error)) => JobOutcome::Failed(format!("{error:#}")),
            Err(_) => JobOutcome::TimedOut,
        },
    };
    finish(&state, &id, outcome).await;
}

async fn signal_shutdown(state: &WebhookState) {
    let mut jobs = state.jobs.write().await;
    state.shutdown.send_replace(true);
    state.job_slots.close();
    for job in jobs
        .values_mut()
        .filter(|job| !job.snapshot.status.is_finished())
    {
        job.snapshot.cancellation_requested = true;
        job.cancel.send_replace(true);
    }
}

async fn shutdown_jobs(state: &WebhookState) {
    signal_shutdown(state).await;
    let mut tasks = std::mem::take(
        &mut *state
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
        while tasks.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tasks.abort_all();
        // Do not await non-cooperative application code indefinitely.
        let mut jobs = state.jobs.write().await;
        for job in jobs
            .values_mut()
            .filter(|job| !job.snapshot.status.is_finished())
        {
            job.snapshot.status = JobState::Cancelled;
            job.snapshot.finished_at_unix = Some(unix_now());
            job.finished_at = Some(Instant::now());
            job.snapshot.error = Some("job cancelled during server shutdown".into());
        }
        evict_finished_jobs(&mut jobs, state.config.webhook.max_retained_jobs);
    }
}

async fn execute_job(state: &WebhookState, request: WebhookTaskRequest) -> Result<AgentResult> {
    let environment = state.config.environment_for(&request.environment)?.clone();
    let mut settings = state.config.agent.clone();
    if let Some(model) = environment.model {
        settings.model = model;
    }
    if let Some(instructions) = environment.instructions {
        settings.instructions = instructions;
    }

    let policy = state
        .config
        .policy_for(&request.user, &[])
        .with_restrictions(
            environment.allowed_tools.as_deref(),
            &environment.disabled_tools,
        );
    let approval: Arc<dyn ApprovalHandler> = if environment.auto_approve_mcp {
        Arc::new(AlwaysApprove)
    } else {
        // A webhook has no interactive terminal. The secure default is to
        // deny approval requests unless the named environment opts in.
        Arc::new(DenyApproval)
    };
    let agent = Agent::new(
        state.client.clone(),
        settings,
        Arc::clone(&state.mcp),
        state.registry.clone(),
        policy,
        approval,
    );
    let context = ToolContext {
        user_id: request.user,
        environment: request.environment,
        workspace: environment.workspace,
        allow_writes: environment.allow_writes,
        checks: environment.checks,
    };
    let mut input = vec![InputPart::Text(request.task)];
    input.extend(
        request
            .images
            .into_iter()
            .map(|image| InputPart::ImageData {
                data: image.data,
                mime_type: image.mime_type,
            }),
    );
    input.extend(request.audio.into_iter().map(|audio| InputPart::AudioData {
        data: audio.data,
        format: audio.format,
    }));

    agent.run(RunRequest { input, context }).await
}

async fn finish(state: &WebhookState, id: &str, outcome: JobOutcome) {
    let mut jobs = state.jobs.write().await;
    if let Some(job) = jobs.get_mut(id) {
        let status = &mut job.snapshot;
        if status.status.is_finished() {
            return;
        }
        // A cancellation acknowledged before completion wins a simultaneous
        // success. Finished jobs are immutable and repeated cancellation is safe.
        let outcome = if status.cancellation_requested {
            JobOutcome::Cancelled
        } else {
            outcome
        };
        status.finished_at_unix = Some(unix_now());
        job.finished_at = Some(Instant::now());
        match outcome {
            JobOutcome::Completed(text, plan) => {
                status.status = match plan.outcome() {
                    RunOutcome::Completed => JobState::Completed,
                    RunOutcome::Blocked => JobState::Blocked,
                    RunOutcome::Incomplete => JobState::Incomplete,
                };
                status.result = Some(text);
                status.plan = Some(plan);
            }
            JobOutcome::Failed(error) => {
                status.status = JobState::Failed;
                status.error = Some(error);
            }
            JobOutcome::Cancelled => {
                status.status = JobState::Cancelled;
                status.cancellation_requested = true;
                status.error =
                    Some("job cancelled; previously completed actions are not undone".into());
            }
            JobOutcome::TimedOut => {
                status.status = JobState::TimedOut;
                status.error = Some(format!(
                    "job exceeded webhook.job_timeout_secs ({} seconds)",
                    state.config.webhook.job_timeout_secs
                ));
            }
        }
    }
    evict_finished_jobs(&mut jobs, state.config.webhook.max_retained_jobs);
}

/// Drop the oldest finished jobs so at most `max_retained` remain.
fn evict_finished_jobs(jobs: &mut HashMap<String, JobRecord>, max_retained: usize) {
    let mut finished = jobs
        .values()
        .filter(|job| job.snapshot.status.is_finished())
        .map(|job| (job.finished_at, job.snapshot.id.clone()))
        .collect::<Vec<_>>();
    let excess = finished.len().saturating_sub(max_retained);
    finished.sort();
    for (_, id) in finished.into_iter().take(excess) {
        jobs.remove(&id);
    }
}

/// Verify `X-Ano-Signature: sha256=<hex>` over `"<timestamp>.<payload>"`,
/// where `<timestamp>` is the `X-Ano-Timestamp` header in Unix seconds.
///
/// Returns the verified signature bytes, or `None` when authentication is
/// disabled.
fn verify_signature(
    state: &WebhookState,
    headers: &HeaderMap,
    payload: &[u8],
    now: u64,
) -> std::result::Result<Option<Vec<u8>>, &'static str> {
    let Some(secret) = state.secret.as_deref() else {
        return if state.config.webhook.allow_unauthenticated {
            Ok(None)
        } else {
            Err("webhook authentication is not configured")
        };
    };
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());

    let timestamp = header(TIMESTAMP_HEADER).ok_or("missing X-Ano-Timestamp")?;
    let issued_at = timestamp
        .parse::<u64>()
        .map_err(|_| "invalid X-Ano-Timestamp")?;
    if now.abs_diff(issued_at) > state.config.webhook.signature_tolerance_secs {
        return Err("expired webhook signature");
    }

    let signature = header(SIGNATURE_HEADER).ok_or("missing X-Ano-Signature")?;
    let signature = signature.strip_prefix("sha256=").unwrap_or(signature);
    let provided = hex::decode(signature).map_err(|_| "invalid webhook signature")?;
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| "invalid webhook secret")?;
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(payload);
    mac.verify_slice(&provided)
        .map_err(|_| "invalid webhook signature")?;
    Ok(Some(provided))
}

/// Record a signature for the tolerance window. Returns `false` if it was
/// already used.
fn remember_signature(state: &WebhookState, signature: Vec<u8>) -> bool {
    let now = unix_now();
    // Expire after the widest window in which the timestamp is still valid.
    let expires_at = now.saturating_add(
        state
            .config
            .webhook
            .signature_tolerance_secs
            .saturating_mul(2),
    );
    let mut seen = state
        .seen_signatures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    seen.retain(|_, expiry| *expiry > now);
    seen.insert(signature, expires_at).is_none()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::{
        cancel_job, create_job, evict_finished_jobs, finish, remember_signature, router,
        shutdown_jobs, unix_now, verify_signature, HmacSha256, JobOutcome, JobRecord, JobState,
        JobStatus, WebhookState,
    };
    use crate::{AppConfig, OpenAiClient, TaskPlan, ToolRegistry};
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

    fn state() -> WebhookState {
        let mut config = AppConfig::default();
        config
            .environments
            .insert("default".into(), Default::default());
        WebhookState {
            config,
            client: OpenAiClient::new("test", "http://127.0.0.1:1234/v1"),
            registry: ToolRegistry::new(),
            mcp: Arc::new(crate::McpPool::new(Vec::new())),
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
                if payload["input"][0]["content"][0]["text"] == "hang" {
                    observed.notify_one();
                    return std::future::pending::<Json<Value>>().await;
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
        state.client = client;
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
    async fn job_timeout_releases_capacity_for_queued_work() {
        let (client, started, server) = api_fixture().await;
        let mut state = state();
        state.client = client;
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
        state.client = client;
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
        state.client = client;
        state
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
        finish(
            &state,
            &id,
            JobOutcome::Completed("late result".into(), TaskPlan::default()),
        )
        .await;
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
        finish(
            &state,
            &old,
            JobOutcome::Completed("old".into(), TaskPlan::default()),
        )
        .await;
        assert_eq!(state.jobs.read().await.len(), 3);
        finish(
            &state,
            &new,
            JobOutcome::Completed("new".into(), TaskPlan::default()),
        )
        .await;
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
            finish(
                &state,
                &id,
                JobOutcome::Completed("Still needs verification".into(), plan),
            )
            .await;
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
                    error: None,
                },
                cancel: watch::channel(false).0,
                finished_at: None,
            },
        );

        let response =
            create_job(State(Arc::clone(&state)), headers.clone(), payload.clone()).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(state.seen_signatures.lock().unwrap().is_empty());

        state.jobs.write().await.remove("existing");
        let response =
            create_job(State(Arc::clone(&state)), headers.clone(), payload.clone()).await;
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
                error: None,
            },
            cancel: watch::channel(false).0,
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
}
