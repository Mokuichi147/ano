//! Signed webhook server that queues tasks as jobs in named environments.

mod jobs;
mod settings;
mod signature;
#[cfg(test)]
mod tests;

pub use jobs::{JobState, JobStatus};
pub use settings::WebhookSettings;

use crate::{
    application::{
        agent::{Agent, AgentResult, RunRequest},
        input::InputPart,
        ports::{McpGateway, ResponsesApi},
        registry::ToolRegistry,
    },
    config::AppConfig,
    domain::plan::RunOutcome,
    infrastructure::project::read_project_instructions,
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
use jobs::{evict_finished_jobs, JobOutcome, JobProgress, JobRecord};
use serde::Deserialize;
use serde_json::json;
use signature::{remember_signature, verify_signature};
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

struct WebhookState {
    config: AppConfig,
    client: Arc<dyn ResponsesApi>,
    registry: ToolRegistry,
    /// MCP connections shared by every job.
    mcp: Arc<dyn McpGateway>,
    jobs: RwLock<HashMap<String, JobRecord>>,
    job_slots: Arc<Semaphore>,
    shutdown: watch::Sender<bool>,
    tasks: Mutex<JoinSet<()>>,
    secret: Option<Vec<u8>>,
    /// Signatures of accepted task requests -> expiry, to reject replays.
    seen_signatures: Mutex<HashMap<Vec<u8>, u64>>,
}

/// Start the inbound webhook server and run until Ctrl+C.
///
/// The server accepts a signed JSON task, queues it, and runs the agent in a
/// named environment from configuration. It intentionally does not allow the
/// caller to submit a raw filesystem path or tool allowlist.
///
/// Every job shares `mcp`. The caller owns it and shuts it down after this
/// returns.
pub async fn serve(
    config: AppConfig,
    client: impl ResponsesApi + 'static,
    mcp: Arc<dyn McpGateway>,
    registry: ToolRegistry,
) -> Result<()> {
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

    let state = Arc::new(WebhookState {
        job_slots: Arc::new(Semaphore::new(webhook.max_concurrent_jobs)),
        mcp,
        config,
        client: Arc::new(client),
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
                    usage: None,
                    stop_reason: None,
                    error: None,
                    recent_events: Vec::new(),
                },
                cancel,
                finished_at: None,
                progress: Default::default(),
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
    match state.jobs.read().await.get(&id).map(|job| job.status()) {
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
        return (StatusCode::OK, Json(job.status())).into_response();
    }
    job.snapshot.cancellation_requested = true;
    job.cancel.send_replace(true);
    (StatusCode::ACCEPTED, Json(job.status())).into_response()
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
    let progress = {
        let mut jobs = state.jobs.write().await;
        let Some(job) = jobs.get_mut(&id) else { return };
        if !job.snapshot.cancellation_requested && !*state.shutdown.borrow() {
            job.snapshot.status = JobState::Running;
            job.snapshot.started_at_unix = Some(unix_now());
        }
        Arc::clone(&job.progress)
    };
    let outcome = tokio::select! {
        biased;
        _ = cancellation_requested(cancellation) => JobOutcome::Cancelled,
        _ = cancellation_requested(shutdown) => JobOutcome::Cancelled,
        outcome = tokio::time::timeout(
            Duration::from_secs(state.config.webhook.job_timeout_secs),
            execute_job(&state, request, progress),
        ) => match outcome {
            Ok(Ok(result)) => JobOutcome::Completed(Box::new(result)),
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

async fn execute_job(
    state: &WebhookState,
    request: WebhookTaskRequest,
    progress: Arc<Mutex<JobProgress>>,
) -> Result<AgentResult> {
    let mut profile = state
        .config
        .execution_profile(&request.user, &request.environment, &[])?;
    if let Some(workspace) = profile.context.workspace.clone() {
        let names = profile.settings.project_instructions.clone();
        let sources =
            tokio::task::spawn_blocking(move || read_project_instructions(&workspace, &names))
                .await
                .context("project instructions task failed")??;
        profile.settings.append_project_instructions(&sources);
    }
    // A webhook has no interactive terminal. The secure default is to deny
    // approval requests unless the named environment opts in.
    let approval = profile.unattended_approval();
    let agent = Agent::new(
        Arc::clone(&state.client),
        profile.settings,
        Arc::clone(&state.mcp),
        state.registry.clone(),
        profile.policy,
        approval,
    )
    .with_event_listener(Arc::new(move |event| {
        progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(event)
    }));
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

    agent
        .run(RunRequest {
            input,
            context: profile.context,
        })
        .await
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
            JobOutcome::Completed(result) => {
                status.status = match result.outcome {
                    RunOutcome::Completed => JobState::Completed,
                    RunOutcome::Blocked => JobState::Blocked,
                    RunOutcome::Incomplete => JobState::Incomplete,
                };
                status.result = Some(result.text);
                status.plan = Some(result.plan);
                status.usage = Some(result.usage);
                status.stop_reason = Some(result.stop_reason);
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

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
