//! `ano acp`: the agent over the Agent Client Protocol. An editor that
//! speaks ACP starts `ano acp` and talks JSON-RPC with it over stdin and
//! stdout; each session works in the folder the editor gives, with the
//! permissions of the command line or of a configured environment.
//!
//! Only protocol messages go to stdout. Warnings go to stderr, which editors
//! keep in their logs.

mod approval;
mod session;
mod updates;

use crate::{
    application::{ports::McpGateway, registry::ToolRegistry},
    config::{AppConfig, DEFAULT_PRESET},
    domain::approval::ApprovalMode,
    harness::Harness,
};
use agent_client_protocol::{
    schema::v1::{
        AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse,
        ConfigOptionUpdate, CurrentModeUpdate, ErrorCode, Implementation, InitializeRequest,
        InitializeResponse, McpCapabilities, NewSessionRequest, NewSessionResponse,
        PromptCapabilities, PromptRequest, PromptResponse, SessionCapabilities,
        SessionCloseCapabilities, SessionId, SessionNotification, SessionUpdate,
        SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
        SetSessionModeResponse,
    },
    Agent, Client, ConnectionTo, Error, Responder, Stdio,
};
use anyhow::{anyhow, Result};
use session::{AcpSession, Workbench, APPROVAL_OPTION, PRESET_OPTION};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How `ano acp` builds its sessions.
#[derive(Debug, Clone)]
pub struct AcpOptions {
    /// The user whose policy and skills sessions use.
    pub user: String,
    /// A configured environment, whose permissions and approval mode the
    /// sessions take instead of the options below.
    pub environment: Option<String>,
    pub allow_writes: bool,
    pub allow_exec: bool,
    /// Defaults to `[agent].approval_mode`. The editor may change it in a
    /// session unless `environment` sets it.
    pub approval_mode: Option<ApprovalMode>,
    /// A preset from `[presets]` that sessions start with.
    pub preset: Option<String>,
    pub disabled_tools: Vec<String>,
}

/// Sends a session's updates to the editor.
#[derive(Clone)]
struct Notifier {
    connection: ConnectionTo<Client>,
    session_id: SessionId,
}

impl Notifier {
    /// Send `update`. Once the editor has gone, there is nobody to tell.
    fn update(&self, update: SessionUpdate) {
        self.connection
            .send_notification(SessionNotification::new(self.session_id.clone(), update))
            .ok();
    }
}

struct AcpState {
    harness: Harness,
    config: AppConfig,
    mcp: Arc<dyn McpGateway>,
    options: AcpOptions,
    sessions: RwLock<HashMap<String, Arc<AcpSession>>>,
}

impl AcpState {
    fn session(&self, id: &SessionId) -> Result<Arc<AcpSession>, Error> {
        self.sessions
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&id.to_string())
            .cloned()
            .ok_or_else(|| invalid_params(anyhow!("unknown session {id}")))
    }
}

/// Serve the editor on stdin and stdout until it closes the connection.
/// Sessions share `mcp` unless the editor gives them MCP servers; the caller
/// owns it and shuts it down after this returns.
pub async fn serve(
    config: AppConfig,
    mcp: Arc<dyn McpGateway>,
    registry: ToolRegistry,
    options: AcpOptions,
) -> Result<()> {
    config.validate()?;
    // Fail at the start, not at every session.
    if let Some(name) = &options.environment {
        config.execution_profile(&options.user, name, &options.disabled_tools)?;
    }
    if let Some(preset) = options
        .preset
        .as_deref()
        .filter(|name| *name != DEFAULT_PRESET)
    {
        config.preset(preset)?;
    }
    let state = Arc::new(AcpState {
        harness: Harness::with_registry(registry, &config)?,
        config,
        mcp,
        options,
        sessions: RwLock::default(),
    });
    let served = Agent
        .builder()
        .name("ano")
        .on_receive_request(
            async move |request: InitializeRequest,
                        responder: Responder<InitializeResponse>,
                        _connection: ConnectionTo<Client>| {
                responder.respond(initialize(&request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: NewSessionRequest,
                            responder: Responder<NewSessionResponse>,
                            connection: ConnectionTo<Client>| {
                    let state = Arc::clone(&state);
                    let spawner = connection.clone();
                    spawner.spawn(async move {
                        responder
                            .respond_with_result(new_session(&state, request, connection).await)
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: PromptRequest,
                            responder: Responder<PromptResponse>,
                            connection: ConnectionTo<Client>| {
                    let session = state.session(&request.session_id)?;
                    // Started here, in the order of the editor's messages, so
                    // a `session/cancel` right after it finds the turn.
                    let turn = session.start_turn(request.prompt).map_err(invalid_params)?;
                    connection.spawn(async move {
                        let result = turn.await.map_err(|error| {
                            Error::new(ErrorCode::InternalError.into(), format!("{error:#}"))
                        });
                        responder.respond_with_result(result.map(PromptResponse::new))
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let state = Arc::clone(&state);
                async move |notification: CancelNotification, _connection: ConnectionTo<Client>| {
                    if let Ok(session) = state.session(&notification.session_id) {
                        session.cancel();
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: CloseSessionRequest,
                            responder: Responder<CloseSessionResponse>,
                            connection: ConnectionTo<Client>| {
                    let removed = state
                        .sessions
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&request.session_id.to_string());
                    let Some(session) = removed else {
                        return Err(invalid_params(anyhow!(
                            "unknown session {}",
                            request.session_id
                        )));
                    };
                    // Answer once the turn has stopped. Completed actions are
                    // not undone.
                    connection.spawn(async move {
                        session.end().await;
                        responder.respond(CloseSessionResponse::new())
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: SetSessionModeRequest,
                            responder: Responder<SetSessionModeResponse>,
                            connection: ConnectionTo<Client>| {
                    let session = state.session(&request.session_id)?;
                    session
                        .set_mode(&request.mode_id.to_string())
                        .map_err(invalid_params)?;
                    responder.respond(SetSessionModeResponse::new())?;
                    // Keep the editor's other view of the mode in step.
                    connection.send_notification(SessionNotification::new(
                        request.session_id,
                        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                            session.config_options(&state.config),
                        )),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: SetSessionConfigOptionRequest,
                            responder: Responder<SetSessionConfigOptionResponse>,
                            connection: ConnectionTo<Client>| {
                    let session = state.session(&request.session_id)?;
                    let value = request
                        .value
                        .as_value_id()
                        .map(ToString::to_string)
                        .ok_or_else(|| invalid_params(anyhow!("the value must be an id")))?;
                    match request.config_id.to_string().as_str() {
                        APPROVAL_OPTION => {
                            session.set_mode(&value).map_err(invalid_params)?;
                            connection.send_notification(SessionNotification::new(
                                request.session_id.clone(),
                                SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(value)),
                            ))?;
                        }
                        PRESET_OPTION => session
                            .set_preset(&state.config, &value)
                            .map_err(invalid_params)?,
                        other => return Err(invalid_params(anyhow!("unknown setting '{other}'"))),
                    }
                    responder.respond(SetSessionConfigOptionResponse::new(
                        session.config_options(&state.config),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await;
    end_sessions(&state).await;
    served.map_err(|error| anyhow!("the ACP connection failed: {error}"))
}

fn initialize(request: &InitializeRequest) -> InitializeResponse {
    InitializeResponse::new(request.protocol_version)
        .agent_capabilities(
            AgentCapabilities::new()
                .prompt_capabilities(
                    PromptCapabilities::new()
                        .image(true)
                        .audio(true)
                        .embedded_context(true),
                )
                .mcp_capabilities(McpCapabilities::new().http(true))
                .session_capabilities(
                    SessionCapabilities::new().close(SessionCloseCapabilities::new()),
                ),
        )
        .agent_info(Implementation::new("ano", env!("CARGO_PKG_VERSION")).title("ano"))
}

async fn new_session(
    state: &AcpState,
    request: NewSessionRequest,
    connection: ConnectionTo<Client>,
) -> Result<NewSessionResponse, Error> {
    let id = SessionId::new(uuid::Uuid::new_v4().to_string());
    let notifier = Notifier {
        connection,
        session_id: id.clone(),
    };
    let bench = Workbench {
        config: &state.config,
        harness: &state.harness,
        mcp: &state.mcp,
        options: &state.options,
    };
    let session = AcpSession::open(bench, notifier, &request.cwd, &request.mcp_servers)
        .await
        .map_err(invalid_params)?;
    let mut response = NewSessionResponse::new(id.clone()).modes(session.modes());
    let options = session.config_options(&state.config);
    if !options.is_empty() {
        response = response.config_options(options);
    }
    state
        .sessions
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(id.to_string(), session);
    Ok(response)
}

/// End every session, waiting a while for their turns to stop.
async fn end_sessions(state: &AcpState) {
    let sessions = std::mem::take(
        &mut *state
            .sessions
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    let ended = futures::future::join_all(sessions.values().map(|session| session.end()));
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, ended).await.is_err() {
        eprintln!("warning: a turn did not stop in time");
    }
}

/// `error` as the message of an invalid-params error, which editors show.
fn invalid_params(error: anyhow::Error) -> Error {
    Error::new(ErrorCode::InvalidParams.into(), format!("{error:#}"))
}
