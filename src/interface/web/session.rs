//! A conversation of the web UI: its workspace, permissions, and model, the
//! agent that works in it, and the turns the page starts and cancels.

use super::{
    approval::{PendingApprovals, WebApproval},
    events::{EventLog, Progress, UiEvent},
};
pub(super) use crate::harness::conversation::TurnRefused;
use crate::{
    application::{
        clock::unix_now,
        input::InputPart,
        ports::{ConversationStore, McpGateway},
    },
    config::{AppConfig, ModelRequest, ModelSelection},
    domain::approval::ApprovalMode,
    harness::{
        approval::ApprovalFactory,
        conversation::{Opening, TurnOutcome, Turns},
        profile::{ExecutionProfile, Permissions},
        Harness,
    },
    infrastructure::memory_store::MemoryConversation,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

/// The environment name of sessions that choose their workspace and
/// permissions in the page instead of a configured environment.
pub(super) const WEB_ENVIRONMENT: &str = "web";

/// How the page asks for a new session.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct NewSession {
    /// The working directory. `~` stands for the home directory, and a
    /// relative path starts at the server's workspace. Defaults to the
    /// server's workspace; an environment with a workspace uses its own.
    pub(super) workspace: Option<String>,
    /// A configured environment, whose permissions and approval mode the
    /// session takes.
    pub(super) environment: Option<String>,
    /// A preset from `[presets]`; `default` is the environment's or
    /// `[agent]`'s choice.
    pub(super) preset: Option<String>,
    pub(super) allow_writes: bool,
    pub(super) allow_exec: bool,
    /// Defaults to `[agent].approval_mode` (`auto` unless set).
    pub(super) approval_mode: Option<ApprovalMode>,
    /// Create the workspace when it does not exist. Without it, a missing
    /// workspace that may be created fails with [`MissingWorkspace`], so
    /// the page can ask first.
    pub(super) create_workspace: bool,
}

/// The workspace the page chose does not exist, and may be created at this
/// path.
#[derive(Debug)]
pub(super) struct MissingWorkspace(pub(super) PathBuf);

impl std::fmt::Display for MissingWorkspace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the workspace does not exist: {}",
            self.0.display()
        )
    }
}

impl std::error::Error for MissingWorkspace {}

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

pub(super) struct WebSession {
    pub(super) info: SessionInfo,
    pub(super) log: Arc<EventLog>,
    pub(super) approvals: Arc<PendingApprovals>,
    turns: Turns,
}

/// What sessions are built from: shared by the sessions of a server.
pub(super) struct Workbench<'a> {
    pub(super) config: &'a AppConfig,
    pub(super) harness: &'a Harness,
    pub(super) mcp: &'a Arc<dyn McpGateway>,
    pub(super) user: &'a str,
    /// The workspace of sessions that name none.
    pub(super) default_workspace: &'a Path,
    /// The folder, canonical, whose subfolders the page may choose as
    /// workspaces; `None` allows any. Environments' own workspaces come from
    /// the config and are not limited.
    pub(super) workspace_root: Option<&'a Path>,
}

impl WebSession {
    /// Resolve `request` and build the session's agent. Nothing contacts the
    /// model until the first turn.
    pub(super) async fn open(bench: Workbench<'_>, request: NewSession) -> Result<Arc<Self>> {
        let (profile, selection, base) = resolve_profile(&bench, &request)?;
        let opening =
            Opening::prepare(bench.harness, bench.config, profile, selection, &base).await?;
        for problem in &opening.skipped_skills {
            eprintln!("warning: skipped skill {problem}");
        }
        let approval_mode = opening.profile.approval_mode;
        let endpoint = opening.endpoint.clone();

        let log = Arc::new(EventLog::default());
        let approvals = Arc::new(PendingApprovals::default());
        let approval = ApprovalFactory {
            mode: approval_mode,
            ask_user: Arc::new(WebApproval {
                log: Arc::clone(&log),
                pending: Arc::clone(&approvals),
            }),
        };
        let events = Arc::clone(&log);
        let text = Arc::clone(&log);
        let conversation = opening.open(bench.harness, Arc::clone(bench.mcp), &approval, |agent| {
            agent
                .with_event_listener(Arc::new(move |event| events.record(event)))
                .with_text_listener(Arc::new(move |delta| text.text_delta(delta)))
        });

        let context = &conversation.context;
        let info = SessionInfo {
            id: Uuid::new_v4().to_string(),
            user: context.user_id.clone(),
            environment: context.environment.clone(),
            workspace: context.workspace.clone(),
            model: conversation.selection.describe(),
            endpoint,
            allow_writes: context.allow_writes,
            allow_exec: context.allow_exec,
            approval_mode,
            created_at_unix: unix_now(),
        };
        Ok(Arc::new(Self {
            info,
            log,
            approvals,
            turns: Turns::new(conversation),
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
        &self,
        text: String,
    ) -> Result<impl Future<Output = ()> + Send + 'static, TurnRefused> {
        let turn = self.turns.start()?;
        self.log.start_turn(text.clone());
        let log = Arc::clone(&self.log);
        Ok(async move {
            let (conversation, outcome) = turn.run(vec![InputPart::Text(text)], None).await;
            let store = &conversation.store;
            let finished = match outcome {
                TurnOutcome::Completed(result) => {
                    let finished = turn_finished(store, Some(&result), None, false);
                    if !result.streamed && !result.text.trim().is_empty() {
                        log.flush_partial();
                        log.push(UiEvent::message(result.text));
                    }
                    finished
                }
                TurnOutcome::Failed(error) => {
                    turn_finished(store, None, Some(format!("{error:#}")), false)
                }
                TurnOutcome::Cancelled(error) => {
                    turn_finished(store, None, error.map(|error| format!("{error:#}")), true)
                }
            };
            log.finish_turn(finished, &store.data().plan, &store.data().usage);
        })
    }

    /// Cancel the running turn. Completed actions are not undone. False
    /// when no turn runs.
    pub(super) fn cancel(&self) -> bool {
        self.turns.cancel()
    }

    /// Refuse further turns and cancel the running one.
    pub(super) fn close(&self) {
        self.turns.close();
    }

    /// End the session: close it, wait until its turn has stopped, and then
    /// tell the pages, so nothing follows `Closed`.
    pub(super) async fn end(&self) {
        self.turns.end().await;
        self.log.push(UiEvent::Closed);
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
        .transpose()?
        .map(|path| bench.default_workspace.join(path));
    // The config chose the workspace, not the page.
    let mut configured = false;
    let (mut profile, base) = match &request.environment {
        Some(name) => {
            if request.allow_writes || request.allow_exec || request.approval_mode.is_some() {
                bail!("environment '{name}' sets the permissions and the approval mode");
            }
            let mut profile = config.execution_profile(bench.user, name, &[])?;
            match (&profile.context.workspace, workspace) {
                (Some(_), Some(_)) => bail!("environment '{name}' has its own workspace"),
                (Some(_), None) => configured = true,
                (None, workspace) => {
                    profile.context.workspace =
                        Some(workspace.unwrap_or_else(|| bench.default_workspace.to_path_buf()))
                }
            }
            (profile, vec![config.environment_request(name)?])
        }
        None => (
            config.ad_hoc_profile(
                bench.user,
                WEB_ENVIRONMENT,
                &[],
                Permissions {
                    workspace: Some(
                        workspace.unwrap_or_else(|| bench.default_workspace.to_path_buf()),
                    ),
                    allow_writes: request.allow_writes,
                    allow_exec: request.allow_exec,
                    approval_mode: request.approval_mode,
                },
            ),
            Vec::new(),
        ),
    };
    let mut layers = base.clone();
    layers.extend(request.preset.as_deref().map(ModelRequest::preset));
    let selection = profile.choose_model(config, &layers)?;
    if let Some(path) = profile.context.workspace.take() {
        let root = bench.workspace_root.filter(|_| !configured);
        let inside_root = |canonical: &Path| {
            match root {
            Some(root) if !canonical.starts_with(root) => bail!(
                "the workspace must be inside {} (start ano web with --allow-any-workspace to choose any folder): {}",
                root.display(),
                path.display()
            ),
            _ => Ok(()),
        }
        };
        let canonical = match std::fs::canonicalize(&path) {
            Ok(canonical) => canonical,
            // A folder the page chose may be created, where it may work.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !configured => {
                let target = new_folder(&path)?;
                inside_root(&target)?;
                if !request.create_workspace {
                    return Err(MissingWorkspace(target).into());
                }
                std::fs::create_dir_all(&target)
                    .with_context(|| format!("failed to create {}", target.display()))?;
                std::fs::canonicalize(&target)?
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "workspace does not exist or cannot be accessed: {}",
                        path.display()
                    )
                })
            }
        };
        if !canonical.is_dir() {
            bail!("workspace is not a directory: {}", path.display());
        }
        inside_root(&canonical)?;
        profile.context.workspace = Some(canonical);
    }
    Ok((profile, selection, base))
}

/// Where the folder `path` would be created: its nearest existing ancestor,
/// resolved, followed by the names of the missing folders. Those must be
/// plain names; a `..` after a folder that does not exist yet could lead
/// anywhere.
fn new_folder(path: &Path) -> Result<PathBuf> {
    let components: Vec<Component<'_>> = path.components().collect();
    let existing = (0..=components.len())
        .rev()
        .find(|&count| count > 0 && components[..count].iter().collect::<PathBuf>().exists())
        .context("no existing folder to create the workspace in")?;
    let mut target = std::fs::canonicalize(components[..existing].iter().collect::<PathBuf>())?;
    for component in &components[existing..] {
        match component {
            Component::Normal(name) => target.push(name),
            _ => bail!(
                "cannot create {}: it goes up from a folder that does not exist",
                path.display()
            ),
        }
    }
    Ok(target)
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
            workspace_root: None,
        };
        resolve_profile(&bench, &request)
    }

    #[test]
    fn missing_folders_are_created_only_when_asked_and_only_inside_the_root() {
        let config =
            AppConfig::parse("[environments.gone]\nworkspace = '/nonexistent-ano-test'").unwrap();
        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let harness = Harness {
            registry: ToolRegistry::new(),
            history: None,
            skills: None,
        };
        let mcp: Arc<dyn McpGateway> = Arc::new(McpPool::new(Vec::new()));
        let resolve = |request: NewSession| {
            let bench = Workbench {
                config: &config,
                harness: &harness,
                mcp: &mcp,
                user: "default",
                default_workspace: root.path(),
                workspace_root: Some(&canonical_root),
            };
            resolve_profile(&bench, &request).map(|(profile, ..)| profile.context.workspace)
        };
        let at = |path: &str, create: bool| NewSession {
            workspace: Some(path.into()),
            create_workspace: create,
            ..NewSession::default()
        };

        // Asked first: nothing is created yet.
        let error = resolve(at("new/project", false)).unwrap_err();
        let missing = error.downcast_ref::<MissingWorkspace>().unwrap();
        assert_eq!(missing.0, canonical_root.join("new/project"));
        assert!(!root.path().join("new").exists());

        let created = resolve(at("new/project", true)).unwrap();
        assert_eq!(created, Some(canonical_root.join("new/project")));
        assert!(root.path().join("new/project").is_dir());

        // Never outside the root, and never by going up from a new folder.
        for path in ["../ano-outside-test", "other/../../ano-outside-test"] {
            let error = resolve(at(path, true)).unwrap_err();
            assert!(error.downcast_ref::<MissingWorkspace>().is_none(), "{path}");
        }
        assert!(!root.path().join("other").exists());
        assert!(!canonical_root
            .parent()
            .unwrap()
            .join("ano-outside-test")
            .exists());

        // The config's workspaces are not created.
        let configured = NewSession {
            environment: Some("gone".into()),
            create_workspace: true,
            ..NewSession::default()
        };
        assert!(resolve(configured).is_err());
        assert!(!Path::new("/nonexistent-ano-test").exists());
    }

    #[test]
    fn the_page_chooses_workspaces_only_inside_the_root_unless_any_is_allowed() {
        let config = AppConfig::parse("[environments.fixed]\nworkspace = '/'").unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("project")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let harness = Harness {
            registry: ToolRegistry::new(),
            history: None,
            skills: None,
        };
        let mcp: Arc<dyn McpGateway> = Arc::new(McpPool::new(Vec::new()));
        let resolve = |workspace_root: Option<&Path>, request: NewSession| {
            let bench = Workbench {
                config: &config,
                harness: &harness,
                mcp: &mcp,
                user: "default",
                default_workspace: root.path(),
                workspace_root,
            };
            resolve_profile(&bench, &request).map(|(profile, ..)| profile.context.workspace)
        };
        let at = |path: &Path| NewSession {
            workspace: Some(path.display().to_string()),
            ..NewSession::default()
        };
        let limited = Some(canonical_root.as_path());

        assert_eq!(
            resolve(limited, NewSession::default()).unwrap(),
            Some(canonical_root.clone())
        );
        assert_eq!(
            resolve(limited, at(&root.path().join("project"))).unwrap(),
            Some(canonical_root.join("project"))
        );
        // A relative path starts at the server's workspace.
        assert_eq!(
            resolve(limited, at(Path::new("project"))).unwrap(),
            Some(canonical_root.join("project"))
        );
        assert!(resolve(limited, at(Path::new(".."))).is_err());
        for escape in [outside.path().to_path_buf(), root.path().join("..")] {
            let error = resolve(limited, at(&escape)).unwrap_err().to_string();
            assert!(error.contains("--allow-any-workspace"), "{error}");
        }
        // The config's environment may work anywhere.
        let fixed = NewSession {
            environment: Some("fixed".into()),
            ..NewSession::default()
        };
        assert!(resolve(limited, fixed).is_ok());
        // Without a root, any folder.
        assert!(resolve(None, at(outside.path())).is_ok());
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
        assert!(context.allow_writes && !context.allow_exec);
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
