//! `ano web`: a browser UI that talks with the agent in sessions, each with
//! its own working directory, permissions, and model.
//!
//! The API is scoped to sessions (`/api/sessions/{id}/...`); for now one
//! session runs at a time. It listens on loopback by default; another
//! address makes it reachable from other machines, over plain HTTP. A
//! browser on this machine needs nothing more; one on another machine opens
//! the URL with the token printed at startup, which then stays in a cookie
//! (`access`).

mod access;
mod approval;
mod events;
mod markdown;
mod session;
#[cfg(test)]
mod tests;

use crate::{
    application::{ports::McpGateway, registry::ToolRegistry},
    config::{AppConfig, DEFAULT_PRESET},
    domain::approval::ApprovalMode,
    harness::Harness,
};
use access::Access;
use anyhow::{bail, Context, Result};
use axum::{
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Redirect, Response,
    },
    routing::{get, post},
    Json, Router,
};
use events::Outgoing;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use session::{NewSession, TurnRefused, WebSession, Workbench};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use tokio::{net::TcpListener, sync::broadcast::error::RecvError, task::JoinSet};

/// Sessions that may exist at the same time. Running several in parallel
/// is the next step; the API and the session table already allow it.
const MAX_SESSIONS: usize = 1;
/// Longest message the page may send.
const MAX_MESSAGE_BYTES: usize = 100_000;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Sets a fixed token instead of a new one at every start.
pub const TOKEN_ENV: &str = "ANO_WEB_TOKEN";

const INDEX_HTML: &str = include_str!("assets/index.html");
const MAIN_JS: &str = include_str!("assets/main.js");
const STYLE_CSS: &str = include_str!("assets/style.css");
/// The UI, built from `web-ui/` with naui by `web-ui/build.sh`.
const UI_JS: &str = include_str!("assets/pkg/ano_web_ui.js");
const UI_WASM: &[u8] = include_bytes!("assets/pkg/ano_web_ui_bg.wasm");
/// Shown to a browser without the token.
const LOCKED_HTML: &str = "<!doctype html><html lang=\"ja\"><meta charset=\"utf-8\"><title>ano</title><link rel=\"stylesheet\" href=\"/assets/style.css\"><body class=\"locked\"><main><h1>ano</h1><p>ano web の起動時に表示された、トークン付きの URL を開いてください。</p></main></body></html>";
/// Scripts come only from this server (WebAssembly may be compiled), and
/// answers cannot load images from elsewhere (which could carry data out in
/// the URL). naui sets a few styles inline.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

/// How `ano web` serves.
#[derive(Debug, Clone)]
pub struct WebOptions {
    /// The address and port to listen on. Other than loopback, other
    /// machines can reach the server (with the token, unless
    /// `authenticate` is off).
    pub bind: String,
    /// The workspace of sessions that name none.
    pub workspace: PathBuf,
    /// The user whose policy and skills sessions use.
    pub user: String,
    /// The token other machines present; `None` makes a new one.
    pub token: Option<String>,
    /// Off, anyone who reaches the server may use it without the token.
    pub authenticate: bool,
}

struct WebState {
    harness: Harness,
    config: AppConfig,
    mcp: Arc<dyn McpGateway>,
    user: String,
    default_workspace: PathBuf,
    access: Access,
    sessions: RwLock<BTreeMap<String, Arc<WebSession>>>,
    /// Serializes session creation, so the limit holds.
    creating: tokio::sync::Mutex<()>,
    turns: Mutex<JoinSet<()>>,
}

impl WebState {
    fn session(&self, id: &str) -> Option<Arc<WebSession>> {
        self.sessions
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }

    fn sessions(&self) -> Vec<Arc<WebSession>> {
        self.sessions
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }
}

/// Serve the web UI until Ctrl+C. Sessions share `mcp`; the caller owns it
/// and shuts it down after this returns.
pub async fn serve(
    config: AppConfig,
    mcp: Arc<dyn McpGateway>,
    registry: ToolRegistry,
    options: WebOptions,
) -> Result<()> {
    config.validate()?;
    let address: SocketAddr = options
        .bind
        .parse()
        .with_context(|| format!("invalid address {}; use IP:PORT", options.bind))?;
    let token = match options.token {
        _ if !options.authenticate => None,
        Some(token) => {
            // The token goes into a URL and a cookie as it is.
            if token.is_empty()
                || !token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
            {
                bail!("{TOKEN_ENV} must be ASCII letters, digits, '-', '_', '.', and '~'");
            }
            Some(token)
        }
        None => Some(uuid::Uuid::new_v4().simple().to_string()),
    };
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to listen on {address}"))?;
    let local = listener.local_addr()?;
    let state = Arc::new(WebState {
        harness: Harness::with_registry(registry, &config)?,
        config,
        mcp,
        user: options.user,
        default_workspace: options.workspace,
        access: Access::new(token, local.port()),
        sessions: RwLock::default(),
        creating: tokio::sync::Mutex::new(()),
        turns: Mutex::new(JoinSet::new()),
    });
    println!(
        "{}\nCtrl+C stops the server.",
        startup_message(local, state.access.token())
    );
    if !local.ip().is_loopback() {
        match state.access.token() {
            Some(_) => eprintln!(
                "warning: ano web is reachable from other machines on {local}. The token and the conversation travel unencrypted over HTTP; use it only on networks you trust."
            ),
            None => eprintln!(
                "warning: ano web is reachable from other machines on {local} without authentication: anyone who can reach it can run the agent with its permissions. Use --no-auth only on networks you trust."
            ),
        }
    }
    let shutdown_state = Arc::clone(&state);
    let served = axum::serve(
        listener,
        router(Arc::clone(&state)).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        tokio::signal::ctrl_c().await.ok();
        close_sessions(&shutdown_state).await;
    })
    .await
    .context("web server stopped unexpectedly");
    close_sessions(&state).await;
    let mut turns = std::mem::take(
        &mut *state
            .turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
        while turns.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        turns.abort_all();
    }
    served
}

/// Where to open the UI. A browser on this machine needs no token; other
/// machines get the token's URL. An unspecified address (`0.0.0.0`, `::`)
/// is shown as this machine's loopback, which it also listens on.
fn startup_message(local: SocketAddr, token: Option<&str>) -> String {
    let port = local.port();
    let this_machine = match local.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        IpAddr::V6(ip) if ip.is_unspecified() => Some(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        ip if ip.is_loopback() => Some(ip),
        _ => None,
    };
    match (this_machine, token) {
        (Some(ip), Some(token)) if local.ip().is_unspecified() => format!(
            "ano web UI: http://{}/\nFrom other machines: http://<this machine's address>:{port}/?token={token}",
            SocketAddr::new(ip, port)
        ),
        (Some(ip), _) => format!("ano web UI: http://{}/", SocketAddr::new(ip, port)),
        (None, Some(token)) => format!("ano web UI: http://{local}/?token={token}"),
        (None, None) => format!("ano web UI: http://{local}/"),
    }
}

/// End every session. Dropping them ends their event streams, so the
/// server's graceful shutdown does not wait on pages.
async fn close_sessions(state: &WebState) {
    let sessions = std::mem::take(
        &mut *state
            .sessions
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    for session in sessions.values() {
        session.close();
    }
    let ended = futures::future::join_all(sessions.values().map(|session| session.end()));
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, ended).await.is_err() {
        eprintln!("warning: a turn did not stop in time");
    }
}

fn router(state: Arc<WebState>) -> Router {
    let api = Router::new()
        .route("/options", get(options))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/messages", post(send_message))
        .route("/sessions/{id}/cancel", post(cancel_turn))
        .route("/sessions/{id}/approvals/{approval}", post(answer_approval))
        .route("/sessions/{id}/events", get(session_events))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            require_access,
        ));
    Router::new()
        .route("/", get(index))
        .route("/assets/main.js", get(|| asset("text/javascript", MAIN_JS)))
        .route("/assets/style.css", get(|| asset("text/css", STYLE_CSS)))
        .route(
            "/assets/pkg/ano_web_ui.js",
            get(|| asset("text/javascript", UI_JS)),
        )
        .route(
            "/assets/pkg/ano_web_ui_bg.wasm",
            get(|| asset("application/wasm", UI_WASM)),
        )
        .nest("/api", api)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn security_headers(request: Request, next: Next) -> Response {
    if !access::same_origin(request.method(), request.headers()) {
        return error_response(StatusCode::FORBIDDEN, "cross-origin request refused");
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

async fn asset(content_type: &'static str, body: impl IntoResponse) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn require_access(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if state.access.admits(peer, request.headers()) {
        next.run(request).await
    } else {
        error_response(
            StatusCode::UNAUTHORIZED,
            "open the URL with the token that ano web printed",
        )
    }
}

#[derive(Deserialize)]
struct IndexQuery {
    token: Option<String>,
}

/// The page. Opened with the token, it stores the token in a cookie and
/// reloads without it, so the token leaves the address bar.
async fn index(
    State(state): State<Arc<WebState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<IndexQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(token) = query.token {
        if !state.access.is_token(&token) {
            return (StatusCode::UNAUTHORIZED, Html(LOCKED_HTML)).into_response();
        }
        let Some(Ok(value)) = state
            .access
            .cookie()
            .map(|cookie| HeaderValue::from_str(&cookie))
        else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        let mut response = Redirect::to("/").into_response();
        response.headers_mut().insert(header::SET_COOKIE, value);
        return response;
    }
    if state.access.admits(peer, &headers) {
        ([(header::CACHE_CONTROL, "no-cache")], Html(INDEX_HTML)).into_response()
    } else {
        (StatusCode::UNAUTHORIZED, Html(LOCKED_HTML)).into_response()
    }
}

/// What the page offers when it creates a session.
async fn options(State(state): State<Arc<WebState>>) -> Response {
    let config = &state.config;
    let mut environments: Vec<_> = config
        .environments
        .iter()
        .map(|(name, environment)| {
            json!({
                "name": name,
                "workspace": environment.workspace,
                "allow_writes": environment.allow_writes,
                "allow_exec": environment.allow_exec,
                "allow_web": environment.allow_web,
                "approval_mode": environment.effective_approval_mode(),
            })
        })
        .collect();
    environments.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let presets: Vec<_> = config
        .presets
        .iter()
        .map(|(name, preset)| {
            json!({
                "name": name,
                "summary": preset.summary(),
                "description": preset.description,
            })
        })
        .collect();
    let default = config
        .select_model(&[])
        .map(|selection| selection.describe())
        .unwrap_or_default();
    Json(json!({
        "user": state.user,
        "default_workspace": state.default_workspace,
        "default_approval_mode": config.agent.approval_mode,
        "approval_modes": ApprovalMode::NAMES,
        "default_preset": {"name": DEFAULT_PRESET, "summary": default},
        "presets": presets,
        "environments": environments,
        "max_sessions": MAX_SESSIONS,
    }))
    .into_response()
}

async fn list_sessions(State(state): State<Arc<WebState>>) -> Response {
    let sessions: Vec<_> = state
        .sessions()
        .iter()
        .map(|session| session.status())
        .collect();
    Json(sessions).into_response()
}

async fn create_session(
    State(state): State<Arc<WebState>>,
    Json(request): Json<NewSession>,
) -> Response {
    let _creating = state.creating.lock().await;
    if state.sessions().len() >= MAX_SESSIONS {
        return error_response(
            StatusCode::CONFLICT,
            "only one session can be open for now; end the current session first",
        );
    }
    let bench = Workbench {
        config: &state.config,
        harness: &state.harness,
        mcp: &state.mcp,
        user: &state.user,
        default_workspace: &state.default_workspace,
    };
    match WebSession::open(bench, request).await {
        Ok(session) => {
            let status = session.status();
            state
                .sessions
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(session.id().to_string(), session);
            (StatusCode::CREATED, Json(status)).into_response()
        }
        Err(error) => error_response(StatusCode::BAD_REQUEST, &format!("{error:#}")),
    }
}

async fn get_session(State(state): State<Arc<WebState>>, Path(id): Path<String>) -> Response {
    match state.session(&id) {
        Some(session) => Json(session.status()).into_response(),
        None => session_not_found(),
    }
}

/// End a session, cancelling its turn, and answer once the turn has
/// stopped. Completed actions are not undone.
async fn delete_session(State(state): State<Arc<WebState>>, Path(id): Path<String>) -> Response {
    // No session is created until this one has stopped, so the limit holds.
    let _creating = state.creating.lock().await;
    let removed = state
        .sessions
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&id);
    match removed {
        Some(session) => {
            session.end().await;
            StatusCode::NO_CONTENT.into_response()
        }
        None => session_not_found(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageRequest {
    text: String,
}

async fn send_message(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    Json(request): Json<MessageRequest>,
) -> Response {
    let Some(session) = state.session(&id) else {
        return session_not_found();
    };
    if request.text.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "the message is empty");
    }
    if request.text.len() > MAX_MESSAGE_BYTES {
        return error_response(StatusCode::BAD_REQUEST, "the message is too long");
    }
    let turn = match session.start_turn(request.text) {
        Ok(turn) => turn,
        Err(TurnRefused::Busy) => {
            return error_response(
                StatusCode::CONFLICT,
                "a turn is already running in this session",
            )
        }
        Err(TurnRefused::Closed) => return session_not_found(),
    };
    let mut turns = state
        .turns
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while turns.try_join_next().is_some() {}
    turns.spawn(turn);
    StatusCode::ACCEPTED.into_response()
}

async fn cancel_turn(State(state): State<Arc<WebState>>, Path(id): Path<String>) -> Response {
    match state.session(&id) {
        Some(session) => Json(json!({"cancelled": session.cancel()})).into_response(),
        None => session_not_found(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalAnswer {
    approved: bool,
}

async fn answer_approval(
    State(state): State<Arc<WebState>>,
    Path((id, approval)): Path<(String, String)>,
    Json(answer): Json<ApprovalAnswer>,
) -> Response {
    let Some(session) = state.session(&id) else {
        return session_not_found();
    };
    if session.approvals.answer(&approval, answer.approved) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        error_response(StatusCode::NOT_FOUND, "no such approval request is waiting")
    }
}

/// The session's events as server-sent events: those after the
/// `Last-Event-ID` the browser sends when it reconnects, then live ones.
async fn session_events(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(session) = state.session(&id) else {
        return session_not_found();
    };
    let after = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let (backlog, receiver) = session.log.subscribe(after);
    // The stream must not keep a closed session alive.
    drop(session);
    // A page that falls behind is disconnected; it reconnects and reads the
    // missed events from the log.
    let live = futures::stream::unfold(receiver, |mut receiver| async move {
        match receiver.recv().await {
            Ok(event) => Some((event, receiver)),
            Err(RecvError::Lagged(_) | RecvError::Closed) => None,
        }
    });
    let stream = futures::stream::iter(backlog)
        .chain(live)
        .map(|event| Ok::<_, Infallible>(sse_event(&event)));
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn sse_event(event: &Outgoing) -> Event {
    let sse = Event::default().data(&*event.data);
    match event.id {
        Some(id) => sse.id(id.to_string()),
        None => sse,
    }
}

fn session_not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "session not found")
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}
