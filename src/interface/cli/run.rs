//! Running the agent for `ano run` and `ano chat`: resolving the settings of
//! a run, wiring its adapters, and opening its session.

use super::{output, AgentOptions, InteractiveApproval, RunArgs};
use crate::{
    application::{
        agent::{Agent, ModelTarget, RunRequest},
        approval::DenyApproval,
        input::InputPart,
        ports::{ApprovalHandler, McpGateway},
        profile::{approval_handler, ExecutionProfile},
        registry::ToolRegistry,
    },
    config::{AppConfig, ModelRequest, ModelSelection},
    domain::{
        approval::ApprovalMode,
        plan::TaskGoal,
        session::{ModelChoice, SessionBinding},
        tool::ToolContext,
    },
    harness::review::ReviewGate,
    infrastructure::{
        chronotope::Chronotope, mcp::McpPool, project::read_project_instructions,
        session_store::Session, skills::SkillLibrary,
    },
    interface::{Connections, RunModels},
};
use anyhow::{bail, Context, Result};
use std::{
    io::{IsTerminal, Read},
    sync::Arc,
};

/// An agent wired to its adapters for one CLI invocation.
pub(super) struct PreparedAgent {
    pub(super) agent: Agent,
    pub(super) context: ToolContext,
    /// Identifies the conversation's user, environment, and endpoint.
    pub(super) binding: SessionBinding,
    pub(super) session: Option<Session>,
    pub(super) mcp: Arc<dyn McpGateway>,
    /// Prints the answer while it is generated, when streaming.
    pub(super) answer: Option<Arc<output::AnswerStream>>,
    /// The provider and model in use, to switch from in `ano chat`.
    pub(super) selection: ModelSelection,
    /// The choice under the run's own: the environment's. `default` in
    /// `ano chat` and in roles returns to it.
    pub(super) base: Vec<ModelRequest>,
    /// Clients of the providers in use, shared when the model changes.
    pub(super) connections: Connections,
    /// Rebuilds the approval handler when the model changes.
    pub(super) approval: ApprovalFactory,
}

/// What an approval handler is built from, so it can be rebuilt for another
/// client when `ano chat` switches models.
pub(super) struct ApprovalFactory {
    pub(super) mode: ApprovalMode,
    pub(super) ask_user: Arc<dyn ApprovalHandler>,
}

impl ApprovalFactory {
    pub(super) fn build(&self, reviewer: ModelTarget) -> Arc<dyn ApprovalHandler> {
        approval_handler(self.mode, reviewer, Arc::clone(&self.ask_user))
    }
}

/// The resolved settings of a run before its adapters are created.
struct RunContext {
    profile: ExecutionProfile,
    selection: ModelSelection,
    /// The choice under the run's own (the environment's).
    base: Vec<ModelRequest>,
    /// Whether `--preset`, `--provider`, `--model`, or `--reasoning-effort`
    /// chose the model. Only then is the choice saved to a session, or a
    /// session moved to another endpoint.
    explicit: bool,
}

pub(super) fn prepare_agent(
    config: &AppConfig,
    user_id: &str,
    options: &AgentOptions,
    registry: ToolRegistry,
    ask_user: Arc<dyn ApprovalHandler>,
    stdin_is_terminal: bool,
    stream: Option<output::TextFormat>,
) -> Result<PreparedAgent> {
    let RunContext {
        mut profile,
        selection,
        base,
        explicit,
    } = resolve_run_context(config, user_id, options)?;
    if let Some(skills) = SkillLibrary::from_settings(&config.skills, &registry)? {
        for problem in skills.add_to_instructions(&mut profile)? {
            eprintln!("warning: skipped skill {problem}");
        }
    }
    let mut connections = Connections::default();
    let models = RunModels::resolve(config, &base, &selection, &mut connections)?;
    let client = Arc::clone(&models.main.client);
    let ask_user: Arc<dyn ApprovalHandler> = if stdin_is_terminal {
        ask_user
    } else {
        match profile.approval_mode {
            ApprovalMode::Ask => eprintln!(
                "warning: stdin is not a terminal; MCP approval requests will be denied (use --approval-mode auto or allow)"
            ),
            ApprovalMode::Auto => eprintln!(
                "warning: stdin is not a terminal; MCP calls that automatic review does not allow will be denied"
            ),
            ApprovalMode::Allow | ApprovalMode::Deny => {}
        }
        Arc::new(DenyApproval)
    };
    let approval = ApprovalFactory {
        mode: profile.approval_mode,
        ask_user,
    };
    let approval_handler = approval.build(models.approval);
    let ExecutionProfile {
        settings,
        policy,
        context,
        ..
    } = profile;
    let binding = SessionBinding::new(&context, client.base_url())?;
    let session = options
        .session
        .as_ref()
        .map(|path| {
            open_session(
                path,
                &binding,
                options.recover_session,
                explicit.then_some(&selection.choice),
            )
        })
        .transpose()?;
    let mcp: Arc<dyn McpGateway> = Arc::new(McpPool::new(config.mcp_servers.clone()));
    let history = Chronotope::from_settings(&config.history, &registry)?;
    let mut agent = Agent::new(
        client,
        settings,
        Arc::clone(&mcp),
        registry,
        policy,
        approval_handler,
    )
    .with_context_window(models.main.context_window)
    .with_subagent_models(models.subagents)
    .with_extension(Arc::new(ReviewGate::new()));
    if let Some(history) = history {
        agent = agent.with_history(history);
    }
    let answer = stream.map(|format| Arc::new(output::AnswerStream::new(format, !options.quiet)));
    if !options.quiet {
        let verbose = options.verbose;
        let answer = answer.clone();
        agent = agent.with_event_listener(Arc::new(move |event| {
            if let Some(answer) = &answer {
                answer.end_response();
            }
            output::print_event(event, verbose)
        }));
    }
    if let Some(answer) = &answer {
        let answer = Arc::clone(answer);
        agent = agent.with_text_listener(Arc::new(move |delta| answer.push(delta)));
    }
    Ok(PreparedAgent {
        agent,
        context,
        binding,
        session,
        mcp,
        answer,
        selection,
        base,
        connections,
        approval,
    })
}

/// Open a saved conversation. With an explicitly chosen model, the choice is
/// saved, and a conversation saved with another endpoint moves to this one.
/// Without one, a conversation stays on the endpoint it was saved with.
fn open_session(
    path: &std::path::Path,
    binding: &SessionBinding,
    recover: bool,
    choice: Option<&ModelChoice>,
) -> Result<Session> {
    let saved = Session::inspect(path).ok().map(|data| data.binding);
    let moves = saved.as_ref().is_some_and(|saved| {
        saved.endpoint != binding.endpoint
            && SessionBinding {
                endpoint: binding.endpoint.clone(),
                ..saved.clone()
            } == *binding
    });
    let Some(choice) = choice else {
        if moves {
            let saved = saved.map(|saved| saved.endpoint).unwrap_or_default();
            bail!("the session is on {saved}, not {}; pass --provider (and --model) to continue it there", binding.endpoint);
        }
        return Session::open(path, binding.clone(), recover);
    };
    let mut opened = binding.clone();
    if moves {
        opened.endpoint = saved.map(|saved| saved.endpoint).unwrap_or_default();
    }
    let mut session = Session::open(path, opened, recover)?;
    crate::application::ports::ConversationStore::switch_model(
        &mut session,
        choice,
        &binding.endpoint,
    )?;
    Ok(session)
}

pub(super) async fn run_agent(
    config: AppConfig,
    user_id: String,
    args: RunArgs,
    registry: ToolRegistry,
) -> Result<()> {
    let stdin_is_terminal = std::io::stdin().is_terminal();
    // Validate options that need no network before reading a piped prompt.
    resolve_run_context(&config, &user_id, &args.agent)?;
    // A piped prompt is trimmed for the model; the raw history keeps it as
    // received.
    let (prompt, piped) = match args.prompt {
        Some(prompt) => (Some(prompt), None),
        // Only read a piped prompt; never block waiting on an interactive
        // terminal when the user supplied only --image or --audio.
        None if !stdin_is_terminal => match read_stdin_prompt()? {
            Some(raw) => (Some(raw.trim().to_string()), Some(raw)),
            None => (None, None),
        },
        None => (None, None),
    };
    let goal = args
        .goal
        .as_deref()
        .map(TaskGoal::from_user)
        .transpose()
        .context("invalid --goal")?;
    if prompt.is_none() && args.images.is_empty() && args.audio.is_empty() && goal.is_none() {
        anyhow::bail!("provide a prompt, --goal, --image, or --audio");
    }
    let PreparedAgent {
        agent,
        context,
        mut session,
        mcp,
        ..
    } = prepare_agent(
        &config,
        &user_id,
        &args.agent,
        registry,
        Arc::new(InteractiveApproval),
        stdin_is_terminal,
        // Show the answer while it is generated, unless it goes to a pipe or
        // into a JSON document.
        (!args.json && std::io::stdout().is_terminal())
            .then(|| output::TextFormat::for_stdout(args.agent.raw)),
    )?;

    let piped = piped.filter(|raw| Some(raw) != prompt.as_ref());
    let mut input = Vec::new();
    if let Some(prompt) = prompt {
        input.push(InputPart::Text(prompt));
    }
    input.extend(args.images.into_iter().map(InputPart::Image));
    input.extend(args.audio.into_iter().map(InputPart::Audio));

    // Only set when it differs from `input`, so attachments are not read twice.
    let raw_input = (piped.is_some() || args.goal.is_some()).then(|| {
        let mut original = input.clone();
        if let Some(raw) = piped {
            original[0] = InputPart::Text(raw);
        }
        if let Some(goal) = args.goal {
            original.push(InputPart::Text(goal));
        }
        original
    });
    let request = RunRequest {
        input,
        raw_input,
        context,
        goal,
    };
    let result = match &mut session {
        Some(session) => agent.run_in_session(request, session).await,
        None => agent.run(request).await,
    };
    output::clear_status();
    // Stop stdio MCP server processes before exiting, even on failure.
    mcp.shutdown().await;
    let result = result?;
    let format = output::TextFormat::for_stdout(args.agent.raw);
    let text = output::format_result(&result, args.json, format)?;
    if !text.is_empty() {
        println!("{text}");
    }
    Ok(())
}

fn resolve_run_context(
    config: &AppConfig,
    user_id: &str,
    args: &AgentOptions,
) -> Result<RunContext> {
    let mut profile = match &args.environment {
        Some(name) => config.execution_profile(user_id, name, &args.disabled_tools)?,
        None => ExecutionProfile {
            settings: config.agent.clone(),
            policy: config.policy_for(user_id, &args.disabled_tools),
            context: ToolContext {
                user_id: user_id.to_string(),
                environment: "cli".to_string(),
                workspace: Some(match &args.workspace {
                    Some(path) => path.clone(),
                    None => {
                        std::env::current_dir().context("failed to determine current directory")?
                    }
                }),
                allow_writes: args.allow_writes,
                allow_exec: args.allow_exec,
                allow_web: args.allow_web,
                checks: Default::default(),
            },
            approval_mode: config.agent.approval_mode,
        },
    };
    if args.non_interactive {
        profile.approval_mode = ApprovalMode::Deny;
    } else if args.auto_approve_mcp {
        profile.approval_mode = ApprovalMode::Allow;
    } else if let Some(mode) = args.approval_mode {
        profile.approval_mode = mode;
    }
    let settings = &mut profile.settings;
    if let Some(threshold) = args.compact_threshold_bytes {
        settings.compact_threshold_bytes = Some(threshold);
    }
    if let Some(rounds) = args.max_tool_rounds {
        settings.max_tool_rounds = rounds;
    }
    if let Some(limit) = args.max_total_tokens {
        settings.max_total_tokens = Some(limit);
    }
    let command_line = ModelRequest {
        preset: args.preset.clone(),
        provider: args.provider.clone(),
        model: args.model.clone(),
        reasoning_effort: args.reasoning_effort.clone(),
    };
    let base = match &args.environment {
        Some(name) => vec![config.environment_request(name)?],
        None => Vec::new(),
    };
    // A resumed session keeps the model it was last switched to, unless the
    // command line chooses another; its effort alone applies over the saved one.
    let saved = match &args.session {
        Some(path) if !command_line.chooses_model() && path.exists() => Session::inspect(path)
            .ok()
            .and_then(|data| data.model)
            .map(|choice| ModelRequest::from(&choice)),
        _ => None,
    };
    let mut layers = base.clone();
    layers.extend(saved.clone());
    layers.push(command_line.clone());
    let selection = config.select_model(&layers).with_context(|| {
        if saved.is_some() {
            "the session's saved provider is unavailable; choose one with --provider or --preset"
        } else {
            "invalid model choice"
        }
    })?;
    selection.apply_to(settings);
    settings.validate()?;
    profile.context.workspace = profile
        .context
        .workspace
        .take()
        .map(|path| {
            let canonical = std::fs::canonicalize(&path).with_context(|| {
                format!(
                    "workspace does not exist or cannot be accessed: {}",
                    path.display()
                )
            })?;
            if !canonical.is_dir() {
                bail!("workspace is not a directory: {}", path.display());
            }
            Ok(canonical)
        })
        .transpose()?;
    if let Some(workspace) = &profile.context.workspace {
        let sources = read_project_instructions(workspace, &profile.settings.project_instructions)?;
        profile.settings.append_project_instructions(&sources);
    }
    Ok(RunContext {
        profile,
        selection,
        base,
        explicit: !command_line.is_empty(),
    })
}

fn read_stdin_prompt() -> Result<Option<String>> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("failed to read prompt from stdin")?;
    Ok((!input.trim().is_empty()).then_some(input))
}

#[cfg(test)]
mod tests {
    use super::super::{Cli, Command};
    use super::*;
    use clap::Parser;

    fn run_args(arguments: &[&str]) -> RunArgs {
        let cli =
            Cli::try_parse_from(std::iter::once("ano").chain(arguments.iter().copied())).unwrap();
        match cli.command {
            Command::Run(args) => args,
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn profile_restrictions_apply_to_cli_runs() {
        let config = AppConfig::parse("[environments.review]\nmodel = 'profile-model'\ninstructions = 'Review only'\nallowed_tools = ['workspace_*']\ndisabled_tools = ['workspace_write']\n[users.default]\nallowed_tools = ['workspace_read', 'echo']").unwrap();
        let args = run_args(&[
            "run",
            "--environment",
            "review",
            "--disable-tool",
            "echo",
            "review",
        ]);
        let ExecutionProfile {
            settings,
            policy,
            context,
            approval_mode,
        } = resolve_run_context(&config, "default", &args.agent)
            .unwrap()
            .profile;
        assert_eq!(settings.model, "profile-model");
        assert_eq!(settings.instructions, "Review only");
        assert!(policy.is_allowed("workspace_read"));
        assert!(!policy.is_allowed("workspace_list"));
        assert!(!policy.is_allowed("echo"));
        assert!(policy.is_disabled("workspace_write"));
        assert!(policy.is_disabled("echo"));
        assert_eq!(context.environment, "review");
        assert!(context.workspace.is_none());
        assert!(!context.allow_writes);
        assert!(!context.allow_exec);
        assert!(!context.allow_web);
        assert_eq!(approval_mode, ApprovalMode::Deny);
    }

    #[test]
    fn cli_cannot_override_profile_permissions() {
        for extra in [
            vec!["--workspace", "."],
            vec!["--allow-writes"],
            vec!["--allow-exec"],
            vec!["--allow-web"],
            vec!["--auto-approve-mcp"],
        ] {
            let mut arguments = vec!["ano", "run", "--environment", "review", "review"];
            arguments.extend(extra);
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        assert!(Cli::try_parse_from([
            "ano",
            "run",
            "hello",
            "--auto-approve-mcp",
            "--non-interactive"
        ])
        .is_err());
    }

    #[test]
    fn invalid_environment_and_workspace_fail_before_api_call() {
        let config = AppConfig::default();
        let args = run_args(&["run", "--environment", "missing", "hello"]);
        assert!(resolve_run_context(&config, "default", &args.agent).is_err());
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        let args = run_args(&["run", "--workspace", missing.to_str().unwrap(), "hello"]);
        assert!(resolve_run_context(&config, "default", &args.agent).is_err());
    }

    #[test]
    fn provider_and_model_choices_carry_over_to_resumed_sessions() {
        let config = AppConfig::parse("[agent]\nmodel = 'gpt-main'\n[providers.local]\nbase_url = 'http://127.0.0.1:1234/v1'\nmodel = 'qwen'\n[providers.lan]\nbase_url = 'http://192.168.1.10:1234/v1'\n[environments.dev]\nprovider = 'local'").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.json");
        let session = path.to_str().unwrap();
        let context = |arguments: &[&str]| {
            let mut all = vec!["run", "--session", session, "--workspace", "."];
            all.extend(arguments);
            all.push("hello");
            resolve_run_context(&config, "default", &run_args(&all).agent).unwrap()
        };
        let open = |run: &RunContext| {
            let binding =
                SessionBinding::new(&run.profile.context, &run.selection.api.base_url).unwrap();
            open_session(
                &path,
                &binding,
                false,
                run.explicit.then_some(&run.selection.choice),
            )
        };

        let first = context(&["--provider", "local"]);
        assert!(first.explicit);
        assert_eq!(first.profile.settings.model, "qwen");
        drop(open(&first).unwrap());

        // Without --provider or --model, the saved choice is used again.
        let resumed = context(&[]);
        assert!(!resumed.explicit);
        assert_eq!(resumed.selection.choice.provider, "local");
        assert_eq!(resumed.profile.settings.model, "qwen");
        drop(open(&resumed).unwrap());

        // Choosing another provider moves the conversation there.
        let moved = context(&["--provider", "lan", "--model", "llama"]);
        assert_eq!(moved.selection.choice.model, "llama");
        let session = open(&moved).unwrap();
        assert_eq!(
            session.data().binding.endpoint,
            "http://192.168.1.10:1234/v1"
        );
        drop(session);

        // A conversation never follows a changed [api] silently.
        let stay = context(&["--provider", "api"]);
        drop(open(&stay).unwrap());
        let mut unchosen = context(&[]);
        unchosen.explicit = false;
        unchosen.selection.api.base_url = "http://elsewhere.invalid/v1".into();
        let error = open(&unchosen).err().unwrap().to_string();
        assert!(error.contains("--provider"), "{error}");

        let environment = resolve_run_context(
            &config,
            "default",
            &run_args(&["run", "--environment", "dev", "hello"]).agent,
        )
        .unwrap();
        assert_eq!(environment.selection.choice.provider, "local");
        assert_eq!(environment.profile.settings.model, "qwen");
        assert!(!environment.explicit);
    }

    #[test]
    fn presets_and_efforts_carry_over_to_resumed_sessions() {
        let config = AppConfig::parse("[agent]\nmodel = 'gpt-main'\n[providers.local]\nbase_url = 'http://127.0.0.1:1234/v1'\nmodel = 'qwen'\n[presets.quick]\nprovider = 'local'\nreasoning_effort = 'low'\n[environments.dev]\npreset = 'quick'\nreasoning_effort = 'medium'").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.json");
        let session = path.to_str().unwrap();
        let context = |arguments: &[&str]| {
            let mut all = vec!["run", "--session", session, "--workspace", "."];
            all.extend(arguments);
            all.push("hello");
            resolve_run_context(&config, "default", &run_args(&all).agent).unwrap()
        };
        let open = |run: &RunContext| {
            let binding =
                SessionBinding::new(&run.profile.context, &run.selection.api.base_url).unwrap();
            drop(
                open_session(
                    &path,
                    &binding,
                    false,
                    run.explicit.then_some(&run.selection.choice),
                )
                .unwrap(),
            );
        };

        let first = context(&["--preset", "quick", "--reasoning-effort", "high"]);
        assert!(first.explicit);
        assert_eq!(first.selection.choice.provider, "local");
        assert_eq!(first.profile.settings.model, "qwen");
        assert_eq!(
            first.profile.settings.reasoning_effort.as_deref(),
            Some("high")
        );
        open(&first);

        let resumed = context(&[]);
        assert_eq!(resumed.selection.choice.provider, "local");
        assert_eq!(
            resumed.profile.settings.reasoning_effort.as_deref(),
            Some("high")
        );

        // Only the effort changes: the saved provider and model stay.
        let lighter = context(&["--reasoning-effort", "minimal"]);
        assert!(lighter.explicit);
        assert_eq!(lighter.selection.choice.provider, "local");
        assert_eq!(lighter.profile.settings.model, "qwen");
        assert_eq!(
            lighter.profile.settings.reasoning_effort.as_deref(),
            Some("minimal")
        );

        // `default` returns to [agent], leaving the saved choice.
        let default = context(&["--preset", "default"]);
        assert_eq!(default.selection.choice.provider, "api");
        assert_eq!(default.profile.settings.reasoning_effort, None);

        let environment = resolve_run_context(
            &config,
            "default",
            &run_args(&["run", "--environment", "dev", "hello"]).agent,
        )
        .unwrap();
        assert_eq!(environment.selection.choice.model, "qwen");
        assert_eq!(
            environment.profile.settings.reasoning_effort.as_deref(),
            Some("medium")
        );
        assert_eq!(
            environment.base,
            [config.environment_request("dev").unwrap()]
        );

        let invalid = run_args(&["run", "--preset", "missing", "hello"]);
        assert!(resolve_run_context(&config, "default", &invalid.agent).is_err());
        let rounds = run_args(&["run", "--max-tool-rounds", "80", "hello"]);
        let run = resolve_run_context(&config, "default", &rounds.agent).unwrap();
        assert_eq!(run.profile.settings.max_tool_rounds, 80);
        let invalid = run_args(&["run", "--max-tool-rounds", "0", "hello"]);
        assert!(resolve_run_context(&config, "default", &invalid.agent).is_err());
        let invalid = run_args(&["run", "--reasoning-effort", "hight", "hello"]);
        assert!(resolve_run_context(&config, "default", &invalid.agent).is_err());
    }
}
