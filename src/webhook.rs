use crate::{
    agent::{Agent, AlwaysApprove, DenyApproval, RunRequest},
    client::OpenAiClient,
    config::AppConfig,
    input::InputPart,
    mcp::McpPool,
    tools::{ToolContext, ToolRegistry},
    ApprovalHandler,
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
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpListener,
    sync::{RwLock, Semaphore},
};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const SIGNATURE_HEADER: &str = "x-ano-signature";
const TIMESTAMP_HEADER: &str = "x-ano-timestamp";
const MAX_TASK_BYTES: usize = 100_000;

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
    Failed,
}

impl JobState {
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub id: String,
    pub status: JobState,
    pub user: String,
    pub environment: String,
    pub created_at_unix: u64,
    pub finished_at_unix: Option<u64>,
    pub result: Option<String>,
    pub error: Option<String>,
}

struct WebhookState {
    config: AppConfig,
    client: OpenAiClient,
    registry: ToolRegistry,
    /// MCP connections shared by every job.
    mcp: Arc<McpPool>,
    jobs: RwLock<HashMap<String, JobStatus>>,
    job_slots: Arc<Semaphore>,
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
        secret,
        seen_signatures: Mutex::new(HashMap::new()),
    });
    let app = Router::new()
        .route(&webhook.path, post(create_job))
        .route("/jobs/{id}", get(get_job))
        .route("/healthz", get(healthz))
        .with_state(state)
        .layer(DefaultBodyLimit::max(webhook.max_body_bytes));

    println!(
        "ano webhook listening on http://{local_addr}{}",
        webhook.path
    );
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
        })
        .await
        .context("webhook server stopped unexpectedly");
    mcp.shutdown().await;
    served
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
            .filter(|job| !job.status.is_finished())
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
        jobs.insert(
            id.clone(),
            JobStatus {
                id: id.clone(),
                status: JobState::Queued,
                user: request.user.clone(),
                environment: request.environment.clone(),
                created_at_unix: unix_now(),
                finished_at_unix: None,
                result: None,
                error: None,
            },
        );
    }

    let task_state = Arc::clone(&state);
    let task_id = id.clone();
    tokio::spawn(async move {
        run_job(task_state, task_id, request).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "job_id": id,
            "status": JobState::Queued,
            "status_url": format!("/jobs/{id}"),
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
    match state.jobs.read().await.get(&id).cloned() {
        Some(status) => (StatusCode::OK, Json(status)).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "job not found"),
    }
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

async fn run_job(state: Arc<WebhookState>, id: String, request: WebhookTaskRequest) {
    // Wait for a free slot while the job stays `queued`.
    let Ok(_permit) = Arc::clone(&state.job_slots).acquire_owned().await else {
        finish(&state, &id, Err("webhook server is shutting down".into())).await;
        return;
    };
    update_status(&state, &id, |status| status.status = JobState::Running).await;

    let outcome = execute_job(&state, request)
        .await
        .map_err(|error| format!("{error:#}"));
    finish(&state, &id, outcome).await;
}

async fn execute_job(state: &WebhookState, request: WebhookTaskRequest) -> Result<String> {
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

    Ok(agent.run(RunRequest { input, context }).await?.text)
}

async fn update_status<F>(state: &WebhookState, id: &str, update: F)
where
    F: FnOnce(&mut JobStatus),
{
    if let Some(status) = state.jobs.write().await.get_mut(id) {
        update(status);
    }
}

async fn finish(state: &WebhookState, id: &str, outcome: std::result::Result<String, String>) {
    update_status(state, id, |status| {
        status.finished_at_unix = Some(unix_now());
        match outcome {
            Ok(text) => {
                status.status = JobState::Completed;
                status.result = Some(text);
            }
            Err(error) => {
                status.status = JobState::Failed;
                status.error = Some(error);
            }
        }
    })
    .await;
}

/// Drop the oldest finished jobs so at most `max_retained` remain.
fn evict_finished_jobs(jobs: &mut HashMap<String, JobStatus>, max_retained: usize) {
    if jobs.len() < max_retained.max(1) {
        return;
    }
    let mut finished = jobs
        .values()
        .filter(|job| job.status.is_finished())
        .map(|job| (job.finished_at_unix.unwrap_or_default(), job.id.clone()))
        .collect::<Vec<_>>();
    finished.sort();
    let excess = jobs.len() + 1 - max_retained.max(1);
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
    let expires_at = now + 2 * state.config.webhook.signature_tolerance_secs;
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
        create_job, evict_finished_jobs, remember_signature, unix_now, verify_signature,
        HmacSha256, JobState, JobStatus, WebhookState,
    };
    use crate::{AppConfig, OpenAiClient, ToolRegistry};
    use axum::{
        body::Bytes,
        extract::State,
        http::{HeaderMap, HeaderValue, StatusCode},
    };
    use hmac::Mac;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use tokio::sync::{RwLock, Semaphore};

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
            JobStatus {
                id: "existing".into(),
                status: JobState::Queued,
                user: "default".into(),
                environment: "default".into(),
                created_at_unix: unix_now(),
                finished_at_unix: None,
                result: None,
                error: None,
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
        let job = |id: &str, status: JobState, finished: Option<u64>| JobStatus {
            id: id.into(),
            status,
            user: "default".into(),
            environment: "default".into(),
            created_at_unix: 0,
            finished_at_unix: finished,
            result: None,
            error: None,
        };
        let mut jobs = HashMap::new();
        for job in [
            job("old", JobState::Completed, Some(1)),
            job("new", JobState::Failed, Some(2)),
            job("running", JobState::Running, None),
        ] {
            jobs.insert(job.id.clone(), job);
        }

        evict_finished_jobs(&mut jobs, 3);
        assert!(!jobs.contains_key("old"));
        assert!(jobs.contains_key("new"));
        assert!(jobs.contains_key("running"));
    }
}
