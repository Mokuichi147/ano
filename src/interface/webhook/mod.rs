//! Signed webhook server that queues tasks as jobs in named environments,
//! and runs the automation of `[automation]`: posted events that become
//! notifications or jobs.

mod automation;
mod jobs;
mod settings;
mod signature;
#[cfg(test)]
mod tests;

pub use automation::{
    AutomationParts, AutomationSettings, NotifierSettings, SourceSettings, TriggerInfo,
};
pub use jobs::{JobState, JobStatus};
pub use settings::WebhookSettings;

use crate::{
    application::{
        agent::{AgentResult, RunRequest},
        approval::DenyApproval,
        automation::{
            unix_now, Admission, AutomationRegistry, PostAuthentication, PostedRequest, Rejection,
        },
        input::InputPart,
        ports::{McpGateway, ResponsesApi},
        registry::ToolRegistry,
    },
    config::AppConfig,
    domain::{
        plan::{RunOutcome, TaskGoal},
        text::cut,
    },
    harness::{
        approval::ApprovalFactory,
        models::{Connections, RunModels},
        notify::{NotifyExtension, AUTOMATION_INSTRUCTIONS, NOTIFY_INSTRUCTIONS},
        Harness,
    },
};
use anyhow::{bail, Context, Result};
use automation::{Automation, JobTrigger};
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
use signature::{forget_signature, remember_signature, verify_signature};
use std::{
    collections::{BTreeMap, HashMap},
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    net::TcpListener,
    sync::{watch, RwLock, Semaphore},
    task::JoinSet,
};
use uuid::Uuid;

const MAX_TASK_BYTES: usize = 100_000;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long shutdown waits to tell of the jobs it cancelled.
const SHUTDOWN_NOTIFY_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a posted event waits for a slot to be handled in.
const EVENT_SLOT_WAIT: Duration = Duration::from_secs(10);
/// The longest reason a source's refusal sends back.
const MAX_REASON_CHARS: usize = 1000;

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
    /// Keep working until this goal is verified as reached.
    #[serde(default)]
    goal: Option<String>,
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

/// A job to run: from a signed task request or from automation.
struct JobSpec {
    user: String,
    environment: String,
    input: Vec<InputPart>,
    goal: Option<String>,
    /// What approvals are judged against, when not the input's text.
    approval_request: Option<String>,
    trigger: Option<Arc<JobTrigger>>,
}

fn default_user() -> String {
    "default".to_string()
}

fn default_environment() -> String {
    "default".to_string()
}

struct WebhookState {
    /// The tools and stores every job's agent shares.
    harness: Harness,
    config: AppConfig,
    /// The client of the default provider (`AppConfig::default_provider`).
    client: Arc<dyn ResponsesApi>,
    /// Clients of the other providers that environments and roles use.
    connections: Connections,
    /// MCP connections shared by every job.
    mcp: Arc<dyn McpGateway>,
    jobs: RwLock<HashMap<String, JobRecord>>,
    job_slots: Arc<Semaphore>,
    shutdown: watch::Sender<bool>,
    tasks: Mutex<JoinSet<()>>,
    secret: Option<Vec<u8>>,
    /// Signatures of accepted task requests -> expiry, to reject replays.
    seen_signatures: Mutex<HashMap<Vec<u8>, u64>>,
    /// The sources, rules, and notifiers of `[automation]`, when any.
    automation: Option<Automation>,
}

/// Start the inbound webhook server and run until Ctrl+C.
///
/// The server accepts a signed JSON task, queues it, and runs the agent in a
/// named environment from configuration. It intentionally does not allow the
/// caller to submit a raw filesystem path or tool allowlist.
///
/// Every job shares `mcp`. The caller owns it and shuts it down after this
/// returns. `[automation]` can use the kinds of sources and notifiers in
/// `automation`.
pub async fn serve(
    config: AppConfig,
    client: impl ResponsesApi + 'static,
    mcp: Arc<dyn McpGateway>,
    registry: ToolRegistry,
    automation: AutomationRegistry,
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

    // Connect to every provider an environment or a role uses up front, so a
    // missing login or API key fails at startup instead of in a job. An
    // environment whose provider or model is disabled fails in its jobs.
    let client: Arc<dyn ResponsesApi> = Arc::new(client);
    let mut connections = Connections::default();
    connections.insert(config.default_provider(), Arc::clone(&client));
    for name in config.environments.keys() {
        let base = [config.environment_request(name)?];
        let Ok(selection) = config.select_model(&base) else {
            continue;
        };
        RunModels::resolve(&config, &base, &selection, &mut connections)
            .with_context(|| format!("failed to connect the providers of environment '{name}'"))?;
    }
    let harness = Harness::with_registry(registry, &config)?;
    let automation_registry = automation;
    let automation = if config.automation.is_enabled() {
        let selection = config.select_model(&[])?;
        let models = RunModels::resolve(&config, &[], &selection, &mut connections)
            .context("failed to connect the provider of the triage model")?;
        let automation = Automation::from_config(&config, &automation_registry, models.triage)?;
        println!(
            "ano automation: {} sources, {} rules; state in {}",
            automation.settings.sources.len(),
            automation.settings.rules.len(),
            automation.store.path().display()
        );
        Some(automation)
    } else {
        None
    };
    let state = Arc::new(WebhookState {
        harness,
        job_slots: Arc::new(Semaphore::new(webhook.max_concurrent_jobs)),
        mcp,
        config,
        client,
        connections,
        jobs: RwLock::new(HashMap::new()),
        shutdown: watch::channel(false).0,
        tasks: Mutex::new(JoinSet::new()),
        secret,
        seen_signatures: Mutex::new(HashMap::new()),
        automation,
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
        .route("/events/{source}", post(post_event))
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
    if let Some(Err(error)) = request.goal.as_deref().map(TaskGoal::from_user) {
        return error_response(StatusCode::BAD_REQUEST, &format!("invalid goal: {error:#}"));
    }
    if !state.config.has_user(&request.user) {
        return error_response(StatusCode::BAD_REQUEST, "unknown user");
    }
    if let Err(error) = state.config.environment_for(&request.environment) {
        return error_response(StatusCode::BAD_REQUEST, &error.to_string());
    }

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
    let spec = JobSpec {
        user: request.user,
        environment: request.environment,
        input,
        goal: request.goal,
        approval_request: None,
        trigger: None,
    };
    match admit_job(&state, spec, signature).await {
        Ok(id) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "job_id": id,
                "status": JobState::Queued,
                "status_url": format!("/jobs/{id}"),
                "cancel_url": format!("/jobs/{id}/cancel"),
            })),
        )
            .into_response(),
        Err((status, message)) => error_response(status, &message),
    }
}

/// Queue `spec` as a job and return its id. A request's `signature` is
/// consumed only when the job is accepted, so a refused request can be
/// sent again.
async fn admit_job(
    state: &Arc<WebhookState>,
    spec: JobSpec,
    signature: Option<Vec<u8>>,
) -> std::result::Result<String, (StatusCode, String)> {
    let refuse = |status: StatusCode, message: &str| Err((status, message.to_string()));
    let id = Uuid::new_v4().to_string();
    let mut jobs = state.jobs.write().await;
    if *state.shutdown.borrow() {
        return refuse(
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
        return refuse(StatusCode::UNAUTHORIZED, "replayed webhook request");
    }
    let pending = jobs
        .values()
        .filter(|job| !job.snapshot.status.is_finished())
        .count();
    if pending >= state.config.webhook.max_pending_jobs {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "too many pending jobs");
    }
    // Consume the signature only when the request can be accepted. Keep
    // this check and insertion under the jobs lock so concurrent retries
    // cannot both enqueue work, while a 503 remains safe to retry.
    if let Some(signature) = signature {
        if !remember_signature(state, signature) {
            return refuse(StatusCode::UNAUTHORIZED, "replayed webhook request");
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
                user: spec.user.clone(),
                environment: spec.environment.clone(),
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
                trigger: spec.trigger.as_ref().map(|trigger| trigger.info()),
            },
            cancel,
            finished_at: None,
            progress: Default::default(),
            trigger: spec.trigger.clone(),
        },
    );
    // Register the task before releasing the admission lock. Shutdown
    // takes the same lock, so no accepted task escapes supervision.
    let task_state = Arc::clone(state);
    let task_id = id.clone();
    let mut tasks = state
        .tasks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while tasks.try_join_next().is_some() {}
    tasks.spawn(async move {
        let run = run_job(Arc::clone(&task_state), task_id.clone(), spec, cancellation);
        if AssertUnwindSafe(run).catch_unwind().await.is_err() {
            finish(
                &task_state,
                &task_id,
                JobOutcome::Failed("job execution panicked".into()),
            )
            .await;
        }
    });
    Ok(id)
}

/// Receive an event posted for the source `source` of `[automation]`. The
/// server checks ano's signature unless the source authenticates requests
/// itself; the source reads the request.
async fn post_event(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    Path(source): Path<String>,
    body: Bytes,
) -> Response {
    let target = state.automation.as_ref().and_then(|automation| {
        let found = automation.source(&source)?;
        Some((automation, found, found.authentication()))
    });
    let signature = match target {
        Some((_, _, PostAuthentication::Source)) => None,
        // Unknown sources are told apart only for requests signed with
        // ano's secret.
        _ => {
            // Domain-separate events from task requests and from other
            // sources.
            let mut payload = format!("event:{source}:").into_bytes();
            payload.extend_from_slice(&body);
            match verify_signature(&state, &headers, &payload, unix_now()) {
                Ok(signature) => signature,
                Err(message) => return error_response(StatusCode::UNAUTHORIZED, message),
            }
        }
    };
    let Some((automation, found, _)) = target else {
        return error_response(StatusCode::NOT_FOUND, "unknown event source");
    };
    let header_values: BTreeMap<String, String> = headers
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
        })
        .collect();
    let received = found.receive(&PostedRequest {
        source: &source,
        headers: &header_values,
        body: &body,
    });
    let mut event = match received {
        Ok(event) => event,
        // A source's reason is cut, so a long one is not sent back whole.
        Err(Rejection::Unauthorized(message)) => {
            return error_response(StatusCode::UNAUTHORIZED, &cut(&message, MAX_REASON_CHARS))
        }
        Err(Rejection::Invalid(message)) => {
            return error_response(StatusCode::BAD_REQUEST, &cut(&message, MAX_REASON_CHARS))
        }
    };
    // A source speaks only for itself.
    event.source.clone_from(&source);
    if let Err(error) = event.check() {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("invalid event: {error:#}"),
        );
    }
    if *state.shutdown.borrow() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "webhook server is shutting down",
        );
    }
    // A full queue is refused before the signature is used, so the same
    // request can be sent again later.
    let Ok(queued) = Arc::clone(&automation.queued_events).try_acquire_owned() else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "too many pending events");
    };
    // The event is recorded only once a slot to handle it is held, so an
    // accepted event never waits recorded but unhandled.
    let Some(slot) = automation.slot(EVENT_SLOT_WAIT).await else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy with other events; send it again later",
        );
    };
    if let Some(signature) = &signature {
        if !remember_signature(&state, signature.clone()) {
            return error_response(StatusCode::UNAUTHORIZED, "replayed webhook request");
        }
    }
    let key = event.key.clone();
    match automation::accept(&state, &event) {
        Admission::New => {}
        Admission::Duplicate => {
            return (StatusCode::OK, Json(json!({"key": key, "duplicate": true}))).into_response()
        }
        Admission::Failed(error) => {
            eprintln!("automation: could not record {source}/{key}: {error}");
            // Nothing was done, so the same request may be sent again.
            if let Some(signature) = &signature {
                forget_signature(&state, signature);
            }
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "the event could not be recorded; send it again later",
            );
        }
    }
    // Triage can take a while; the poster only learns that it arrived.
    let task_state = Arc::clone(&state);
    let mut tasks = state
        .tasks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while tasks.try_join_next().is_some() {}
    tasks.spawn(async move {
        automation::process_in(&task_state, event, slot).await;
        drop(queued);
    });
    drop(tasks);
    (
        StatusCode::ACCEPTED,
        Json(json!({"key": key, "duplicate": false})),
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
    spec: JobSpec,
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
            execute_job(&state, spec, progress),
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
    let take = || {
        std::mem::take(
            &mut *state
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    };
    // Finishing jobs add the tasks that send their notifications, so wait
    // until no task is left.
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
        loop {
            let mut tasks = take();
            if tasks.is_empty() {
                break;
            }
            while tasks.join_next().await.is_some() {}
        }
    })
    .await
    .is_err()
    {
        // Dropping the timed out wait aborted the tasks it held.
        take().abort_all();
        // Do not await non-cooperative application code indefinitely.
        let mut jobs = state.jobs.write().await;
        let mut notifications = Vec::new();
        for job in jobs
            .values_mut()
            .filter(|job| !job.snapshot.status.is_finished())
        {
            job.snapshot.status = JobState::Cancelled;
            job.snapshot.finished_at_unix = Some(unix_now());
            job.finished_at = Some(Instant::now());
            let error = "job cancelled during server shutdown";
            job.snapshot.error = Some(error.into());
            if let Some(trigger) = &job.trigger {
                if trigger.rule.notifies(JobState::Cancelled.name()) {
                    let notifiers = trigger.notifiers.clone();
                    let notification = trigger.finished(JobState::Cancelled.name(), error);
                    notifications
                        .push(async move { automation::send_all(&notifiers, &notification).await });
                }
            }
        }
        evict_finished_jobs(&mut jobs, state.config.webhook.max_retained_jobs);
        drop(jobs);
        // The rules asked to hear of cancelled jobs; the wait stays short.
        let _ = tokio::time::timeout(
            SHUTDOWN_NOTIFY_TIMEOUT,
            futures::future::join_all(notifications),
        )
        .await;
    }
}

async fn execute_job(
    state: &WebhookState,
    spec: JobSpec,
    progress: Arc<Mutex<JobProgress>>,
) -> Result<AgentResult> {
    let mut profile = state
        .config
        .execution_profile(&spec.user, &spec.environment, &[])?;
    if let Some(trigger) = &spec.trigger {
        let instructions = &mut profile.settings.instructions;
        instructions.push_str("\n\n");
        instructions.push_str(AUTOMATION_INSTRUCTIONS);
        if trigger.rule.allows_notify_user(&trigger.event) {
            instructions.push(' ');
            instructions.push_str(NOTIFY_INSTRUCTIONS);
        }
    }
    let base = [state.config.environment_request(&spec.environment)?];
    let selection = state.config.select_model(&base)?;
    selection.apply_to(&mut profile.settings);
    let mut connections = state.connections.clone();
    // The default provider's client, which `serve` was given.
    connections.insert(state.config.default_provider(), Arc::clone(&state.client));
    let models = RunModels::resolve(&state.config, &base, &selection, &mut connections)?;
    let harness = state.harness.clone();
    let profile = tokio::task::spawn_blocking(move || {
        // Broken skill files are left out; they must not fail the job.
        harness.add_instructions(&mut profile).map(|_| profile)
    })
    .await
    .context("instructions task failed")??;
    // A webhook has nobody to ask: requests that would go to the user, and
    // those automatic review does not clearly allow, are denied.
    let approval = ApprovalFactory {
        mode: profile.approval_mode,
        ask_user: Arc::new(DenyApproval),
    };
    let mut agent = state
        .harness
        .agent(&profile, models, Arc::clone(&state.mcp), &approval)
        .with_event_listener(Arc::new(move |event| {
            progress
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .record(event)
        }));
    if let Some(trigger) = spec
        .trigger
        .as_ref()
        .filter(|trigger| trigger.rule.allows_notify_user(&trigger.event))
    {
        agent = agent.with_extension(Arc::new(NotifyExtension::new(
            trigger.notifiers.clone(),
            &trigger.rule.name,
            trigger.event.url.clone(),
        )));
    }
    let input = spec.input;
    let raw_input = spec.goal.as_ref().map(|goal| {
        let mut original = input.clone();
        original.push(InputPart::Text(goal.clone()));
        original
    });
    agent
        .run(RunRequest {
            input,
            raw_input,
            context: profile.context,
            goal: spec.goal.as_deref().map(TaskGoal::from_user).transpose()?,
            approval_request: spec.approval_request,
        })
        .await
}

async fn finish(state: &WebhookState, id: &str, outcome: JobOutcome) {
    let mut jobs = state.jobs.write().await;
    let mut notification = None;
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
        if let Some(trigger) = &job.trigger {
            let name = status.status.name();
            if trigger.rule.notifies(name) {
                let text = status
                    .result
                    .as_deref()
                    .or(status.error.as_deref())
                    .unwrap_or("");
                notification = Some((trigger.notifiers.clone(), trigger.finished(name, text)));
            }
        }
    }
    evict_finished_jobs(&mut jobs, state.config.webhook.max_retained_jobs);
    drop(jobs);
    if let Some((notifiers, notification)) = notification {
        // Sending can be slow; the job's slot is not held for it. Shutdown
        // waits for it like for the jobs.
        state
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .spawn(async move { automation::send_all(&notifiers, &notification).await });
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
