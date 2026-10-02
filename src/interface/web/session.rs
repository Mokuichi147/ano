//! A conversation of the web UI: its workspace, permissions, and model, the
//! agent that works in it, and the turns the page starts and cancels.

use super::{
    approval::{PendingApprovals, WebApproval},
    events::{EventLog, Progress, UiEvent},
};
use crate::{
    application::{
        agent::{Agent, RunRequest},
        input::InputPart,
        ports::{ConversationStore, McpGateway},
    },
    config::{AppConfig, ModelRequest, ModelSelection},
    domain::{
        approval::ApprovalMode,
        session::{SessionBinding, SessionStatus},
        tool::ToolContext,
    },
    harness::{
        approval::ApprovalFactory,
        models::{Connections, RunModels},
        profile::ExecutionProfile,
        Harness,
    },
    infrastructure::memory_store::MemoryConversation,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::oneshot;
use uuid::Uuid;

/// The environment name of sessions that choose their workspace and
/// permissions in the page instead of a configured environment.
pub(super) const WEB_ENVIRONMENT: &str = "web";

/// How the page asks for a new session.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct NewSession {
    /// The working directory. `~` stands for the home directory. Defaults
    /// to the server's; an environment with a workspace uses its own.
    pub(super) workspace: Option<String>,
    /// A configured environment, whose permissions and approval mode the
    /// session takes.
    pub(super) environment: Option<String>,
    /// A preset from `[presets]`; `default` is the environment's or
    /// `[agent]`'s choice.
    pub(super) preset: Option<String>,
    pub(super) allow_writes: bool,
    pub(super) allow_exec: bool,
    pub(super) allow_web: bool,
    /// Defaults to `[agent].approval_mode` (`auto` unless set).
    pub(super) approval_mode: Option<ApprovalMode>,
}

/// What the page shows about a session.
#[derive(Debug, Clone, Serialize)]
pub(super) struct SessionInfo {
    pub(super) id: String,
    pub(super) user: String,
    pub(super) environment: String,
    pub(super) workspace: Option<PathBuf>,
    pub(super) model: String,
    pub(super) endpoint: String,
    pub(super) allow_writes: bool,
    pub(super) allow_exec: bool,
    pub(super) allow_web: bool,
    pub(super) approval_mode: ApprovalMode,
    pub(super) created_at_unix: u64,
}

/// A session with what is happening in it now.
#[derive(Debug, Serialize)]
pub(super) struct SessionStatusView {
    #[serde(flatten)]
    pub(super) info: SessionInfo,
    #[serde(flatten)]
    pub(super) progress: Progress,
}

/// What a turn needs exclusively: one turn runs at a time.
struct Conversation {
    agent: Agent,
    store: MemoryConversation,
    context: ToolContext,
}

pub(super) struct WebSession {
    pub(super) info: SessionInfo,
    pub(super) log: Arc<EventLog>,
    pub(super) approvals: Arc<PendingApprovals>,
    conversation: Arc<tokio::sync::Mutex<Conversation>>,
    control: Mutex<TurnControl>,
}

/// Whether the session ended, and how to cancel its running turn. Kept
/// under one lock, so a turn never starts after the session ends.
#[derive(Default)]
struct TurnControl {
    closed: bool,
    cancel: Option<oneshot::Sender<()>>,
}

/// Why a turn cannot start.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TurnRefused {
    /// Another turn runs.
    Busy,
    /// The session ended.
    Closed,
}

/// What sessions are built from: shared by the sessions of a server.
pub(super) struct Workbench<'a> {
    pub(super) config: &'a AppConfig,
    pub(super) harness: &'a Harness,
    pub(super) mcp: &'a Arc<dyn McpGateway>,
    pub(super) user: &'a str,
    /// The workspace of sessions that name none.
    pub(super) default_workspace: &'a Path,
}

impl WebSession {
    /// Resolve `request` and build the session's agent. Nothing contacts the
    /// model until the first turn.
    pub(super) async fn open(bench: Workbench<'_>, request: NewSession) -> Result<Arc<Self>> {
        let (mut profile, selection, base) = resolve_profile(&bench, &request)?;
        let harness = bench.harness.clone();
        let mut profile = tokio::task::spawn_blocking(move || {
            harness.add_instructions(&mut profile).map(|problems| {
                for problem in problems {
                    eprintln!("warning: skipped skill {problem}");
                }
                profile
            })
        })
        .await
        .context("instructions task failed")??;
        let models =
            RunModels::resolve(bench.config, &base, &selection, &mut Connections::default())?;
        let endpoint = models.main.client.base_url().to_string();
        let binding = SessionBinding::new(&profile.context, &endpoint)?;

        let log = Arc::new(EventLog::default());
        let approvals = Arc::new(PendingApprovals::default());
        let approval = ApprovalFactory {
            mode: profile.approval_mode,
            ask_user: Arc::new(WebApproval {
                log: Arc::clone(&log),
                pending: Arc::clone(&approvals),
            }),
        };
        let events = Arc::clone(&log);
        let text = Arc::clone(&log);
        let agent = bench
            .harness
            .agent(&profile, models, Arc::clone(bench.mcp), &approval)
            .with_event_listener(Arc::new(move |event| events.record(event)))
            .with_text_listener(Arc::new(move |delta| text.text_delta(delta)));

        let context = std::mem::take(&mut profile.context);
        let info = SessionInfo {
            id: Uuid::new_v4().to_string(),
            user: context.user_id.clone(),
            environment: context.environment.clone(),
            workspace: context.workspace.clone(),
            model: selection.describe(),
            endpoint,
            allow_writes: context.allow_writes,
            allow_exec: context.allow_exec,
            allow_web: context.allow_web,
            approval_mode: profile.approval_mode,
            created_at_unix: unix_now(),
        };
        Ok(Arc::new(Self {
            info,
            log,
            approvals,
            conversation: Arc::new(tokio::sync::Mutex::new(Conversation {
                agent,
                store: MemoryConversation::new(binding),
                context,
            })),
            control: Mutex::default(),
        }))
    }

    pub(super) fn id(&self) -> &str {
        &self.info.id
    }

    pub(super) fn status(&self) -> SessionStatusView {
        SessionStatusView {
            info: self.info.clone(),
            progress: self.log.progress(),
        }
    }

    /// Start a turn with the user's `text`. The returned future runs it to
    /// the end; the caller spawns it.
    pub(super) fn start_turn(
        self: &Arc<Self>,
        text: String,
    ) -> Result<impl Future<Output = ()> + Send + 'static, TurnRefused> {
        let mut control = self.lock_control();
        if control.closed {
            return Err(TurnRefused::Closed);
        }
        let mut conversation = Arc::clone(&self.conversation)
            .try_lock_owned()
            .map_err(|_| TurnRefused::Busy)?;
        let (cancel, cancelled) = oneshot::channel();
        control.cancel = Some(cancel);
        drop(control);
        self.log.start_turn(text.clone());
        let log = Arc::clone(&self.log);
        Ok(async move {
            let Conversation {
                agent,
                store,
                context,
            } = &mut *conversation;
            let request = RunRequest {
                input: vec![InputPart::Text(text)],
                raw_input: None,
                context: context.clone(),
                goal: None,
            };
            // A dropped sender cancels as well. Cancellation comes first, so
            // a turn cancelled before it ran does not start.
            let outcome = tokio::select! {
                biased;
                _ = cancelled => None,
                result = agent.run_in_session(request, store) => Some(result),
            };
            let finished = match outcome {
                Some(Ok(result)) => {
                    let finished = turn_finished(store, Some(&result), None, false);
                    if !result.streamed && !result.text.trim().is_empty() {
                        log.flush_partial();
                        log.push(UiEvent::message(result.text));
                    }
                    finished
                }
                // The failure is recorded in the conversation; the next turn
                // continues from it.
                Some(Err(error)) => turn_finished(store, None, Some(format!("{error:#}")), false),
                None => {
                    let error = (store.data().status == SessionStatus::Running)
                        .then(|| store.fail("Interrupted by the user."))
                        .and_then(Result::err)
                        .map(|error| format!("{error:#}"));
                    turn_finished(store, None, error, true)
                }
            };
            log.finish_turn(finished, &store.data().plan, &store.data().usage);
        })
    }

    /// Cancel the running turn. Completed actions are not undone. False
    /// when no turn runs.
    pub(super) fn cancel(&self) -> bool {
        self.lock_control()
            .cancel
            .take()
            .is_some_and(|cancel| cancel.send(()).is_ok())
    }

    /// Refuse further turns and cancel the running one.
    pub(super) fn close(&self) {
        let mut control = self.lock_control();
        control.closed = true;
        if let Some(cancel) = control.cancel.take() {
            cancel.send(()).ok();
        }
    }

    /// End the session: close it, wait until its turn has stopped, and then
    /// tell the pages, so nothing follows `Closed`.
    pub(super) async fn end(&self) {
        self.close();
        drop(self.conversation.lock().await);
        self.log.push(UiEvent::Closed);
    }

    fn lock_control(&self) -> std::sync::MutexGuard<'_, TurnControl> {
        self.control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn turn_finished(
    store: &MemoryConversation,
    result: Option<&crate::application::agent::AgentResult>,
    error: Option<String>,
    cancelled: bool,
) -> UiEvent {
    UiEvent::TurnFinished {
        outcome: result.map(|result| result.outcome),
        stop_reason: result.map(|result| result.stop_reason),
        usage: store.data().usage.clone(),
        error,
        cancelled,
    }
}

/// The settings of a session's runs, its model, and the choice under it
/// (the environment's), as `ano chat` resolves them from its options.
fn resolve_profile(
    bench: &Workbench<'_>,
    request: &NewSession,
) -> Result<(ExecutionProfile, ModelSelection, Vec<ModelRequest>)> {
    let config = bench.config;
    let workspace = request
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(expand_home)
        .transpose()?;
    let (mut profile, base) = match &request.environment {
        Some(name) => {
            if request.allow_writes
                || request.allow_exec
                || request.allow_web
                || request.approval_mode.is_some()
            {
                bail!("environment '{name}' sets the permissions and the approval mode");
            }
            let mut profile = config.execution_profile(bench.user, name, &[])?;
            match (&profile.context.workspace, workspace) {
                (Some(_), Some(_)) => bail!("environment '{name}' has its own workspace"),
                (Some(_), None) => {}
                (None, workspace) => {
                    profile.context.workspace =
                        Some(workspace.unwrap_or_else(|| bench.default_workspace.to_path_buf()))
                }
            }
            (profile, vec![config.environment_request(name)?])
        }
        None => (
            ExecutionProfile {
                settings: config.agent.settings.clone(),
                project_instructions: config.agent.project_instructions.clone(),
                policy: config.policy_for(bench.user, &[]),
                context: ToolContext {
                    user_id: bench.user.to_string(),
                    environment: WEB_ENVIRONMENT.to_string(),
                    workspace: Some(
                        workspace.unwrap_or_else(|| bench.default_workspace.to_path_buf()),
                    ),
                    allow_writes: request.allow_writes,
                    allow_exec: request.allow_exec,
                    allow_web: request.allow_web,
                    checks: Default::default(),
                },
                approval_mode: request.approval_mode.unwrap_or(config.agent.approval_mode),
            },
            Vec::new(),
        ),
    };
    let mut layers = base.clone();
    layers.extend(request.preset.as_deref().map(ModelRequest::preset));
    let selection = config
        .select_model(&layers)
        .context("invalid model choice")?;
    selection.apply_to(&mut profile.settings);
    profile.settings.validate()?;
    if let Some(path) = profile.context.workspace.take() {
        let canonical = std::fs::canonicalize(&path).with_context(|| {
            format!(
                "workspace does not exist or cannot be accessed: {}",
                path.display()
            )
        })?;
        if !canonical.is_dir() {
            bail!("workspace is not a directory: {}", path.display());
        }
        profile.context.workspace = Some(canonical);
    }
    Ok((profile, selection, base))
}

/// `path` with a leading `~` replaced by the home directory.
fn expand_home(path: &str) -> Result<PathBuf> {
    let rest = match path.strip_prefix('~') {
        Some("") => "",
        Some(rest) if rest.starts_with(['/', std::path::MAIN_SEPARATOR]) => &rest[1..],
        _ => return Ok(PathBuf::from(path)),
    };
    let home = std::env::home_dir().context("cannot expand '~': the home directory is unknown")?;
    Ok(home.join(rest))
}

pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::registry::ToolRegistry;
    use crate::infrastructure::mcp::McpPool;

    fn resolve(
        config: &AppConfig,
        default_workspace: &Path,
        request: NewSession,
    ) -> Result<(ExecutionProfile, ModelSelection, Vec<ModelRequest>)> {
        let harness = Harness {
            registry: ToolRegistry::new(),
            history: None,
            skills: None,
        };
        let mcp: Arc<dyn McpGateway> = Arc::new(McpPool::new(Vec::new()));
        let bench = Workbench {
            config,
            harness: &harness,
            mcp: &mcp,
            user: "default",
            default_workspace,
        };
        resolve_profile(&bench, &request)
    }

    #[test]
    fn sessions_work_in_their_own_folder_with_the_permissions_chosen_for_them() {
        let config = AppConfig::parse("[agent]\nmodel = 'gpt-main'\n[providers.local]\nbase_url = 'http://127.0.0.1:1234/v1'\nmodel = 'qwen'\n[presets.quick]\nprovider = 'local'\n[environments.review]\nallowed_tools = ['workspace_*']\n[environments.fixed]\nworkspace = '/'").unwrap();
        let server = tempfile::tempdir().unwrap();
        let chosen = tempfile::tempdir().unwrap();

        let (profile, selection, base) = resolve(
            &config,
            server.path(),
            NewSession {
                workspace: Some(chosen.path().display().to_string()),
                preset: Some("quick".into()),
                allow_writes: true,
                approval_mode: Some(ApprovalMode::Auto),
                ..NewSession::default()
            },
        )
        .unwrap();
        let context = &profile.context;
        assert_eq!(
            context.workspace,
            Some(std::fs::canonicalize(chosen.path()).unwrap())
        );
        assert_eq!(context.environment, WEB_ENVIRONMENT);
        assert!(context.allow_writes && !context.allow_exec && !context.allow_web);
        assert_eq!(profile.approval_mode, ApprovalMode::Auto);
        assert_eq!(selection.choice.provider, "local");
        assert_eq!(profile.settings.model, "qwen");
        assert!(base.is_empty());

        let (profile, ..) = resolve(&config, server.path(), NewSession::default()).unwrap();
        assert_eq!(
            profile.context.workspace,
            Some(std::fs::canonicalize(server.path()).unwrap())
        );
        assert!(!profile.context.allow_writes);
        assert_eq!(profile.approval_mode, ApprovalMode::Auto);

        let (profile, ..) = resolve(
            &config,
            server.path(),
            NewSession {
                environment: Some("review".into()),
                workspace: Some(chosen.path().display().to_string()),
                ..NewSession::default()
            },
        )
        .unwrap();
        assert_eq!(profile.context.environment, "review");
        assert!(!profile.policy.is_allowed("echo"));
        assert_eq!(profile.approval_mode, ApprovalMode::Deny);

        for invalid in [
            NewSession {
                environment: Some("review".into()),
                allow_exec: true,
                ..NewSession::default()
            },
            NewSession {
                environment: Some("fixed".into()),
                workspace: Some(chosen.path().display().to_string()),
                ..NewSession::default()
            },
            NewSession {
                environment: Some("missing".into()),
                ..NewSession::default()
            },
            NewSession {
                workspace: Some(chosen.path().join("missing").display().to_string()),
                ..NewSession::default()
            },
            NewSession {
                preset: Some("missing".into()),
                ..NewSession::default()
            },
        ] {
            assert!(resolve(&config, server.path(), invalid).is_err());
        }
    }

    #[test]
    fn a_leading_tilde_is_the_home_directory() {
        let home = std::env::home_dir().unwrap();
        assert_eq!(expand_home("~").unwrap(), home);
        assert_eq!(expand_home("~/src").unwrap(), home.join("src"));
        assert_eq!(expand_home("~src").unwrap(), PathBuf::from("~src"));
        assert_eq!(expand_home("/tmp").unwrap(), PathBuf::from("/tmp"));
    }
}
