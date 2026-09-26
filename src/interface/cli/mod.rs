//! The `ano` command line. This module is also the composition root of the
//! binary: it builds the infrastructure adapters and wires them into the
//! application layer.

mod approval;
mod chat;
mod mcp;
mod output;

pub use approval::InteractiveApproval;

use crate::{
    application::{
        agent::{Agent, RunRequest},
        approval::DenyApproval,
        input::InputPart,
        ports::{ApprovalHandler, McpGateway},
        profile::ExecutionProfile,
        registry::ToolRegistry,
    },
    config::AppConfig,
    domain::{
        approval::ApprovalMode, mcp::McpTransport, plan::TASK_PLAN_NAME, session::SessionBinding,
        tool::ToolContext,
    },
    infrastructure::{
        mcp::McpPool, openai::OpenAiClient, project::read_project_instructions,
        session_store::Session, tools::register_builtin_tools,
    },
    interface::webhook,
};
use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::{
    io::{IsTerminal, Read},
    path::PathBuf,
    sync::Arc,
};

const DEFAULT_CONFIG_PATH: &str = "config.toml";

#[derive(Debug, Parser)]
#[command(
    name = "ano",
    version,
    about = "Autonomous Rust agent powered by the OpenAI Responses API"
)]
struct Cli {
    /// Config file. Defaults to ./config.toml when present; an explicitly
    /// given path must exist.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[arg(long, global = true, default_value = "default")]
    user: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one task and print the result.
    Run(RunArgs),
    /// Talk with the agent over several turns in one conversation.
    Chat(ChatArgs),
    Tools(ToolsArgs),
    Serve(ServeArgs),
    /// Inspect saved conversation state without contacting the model.
    Session(SessionArgs),
    /// Manage the authorization of MCP servers.
    Mcp(mcp::McpArgs),
}

/// Options shared by `run` and `chat`: environment, permissions, model, and
/// conversation persistence.
#[derive(Debug, Args)]
struct AgentOptions {
    #[arg(
        long,
        value_name = "PATH",
        help = "Save and continue a local conversation"
    )]
    session: Option<PathBuf>,

    #[arg(
        long,
        requires = "session",
        help = "Recover an interrupted session without replaying pending tool calls"
    )]
    recover_session: bool,

    #[arg(
        long,
        help = "Compact history after this many JSON bytes (requires /responses/compact)"
    )]
    compact_threshold_bytes: Option<usize>,

    #[arg(
        long,
        help = "Stop after a response reaches this run's observed token budget (soft limit)"
    )]
    max_total_tokens: Option<u64>,

    #[arg(long = "disable-tool", value_name = "NAME")]
    disabled_tools: Vec<String>,

    #[arg(long)]
    model: Option<String>,

    #[arg(
        long,
        value_name = "LEVEL",
        help = "Reasoning effort: none, minimal, low, medium, high, or xhigh"
    )]
    reasoning_effort: Option<String>,

    #[arg(
        long,
        value_name = "MODE",
        conflicts_with_all = ["auto_approve_mcp", "non_interactive", "environment"],
        help = "How MCP approval requests are answered: ask, auto (a reviewer model decides and asks only when unsure), allow, or deny"
    )]
    approval_mode: Option<ApprovalMode>,

    #[arg(long, conflicts_with_all = ["non_interactive", "environment"], help = "Approve remote MCP calls automatically (same as --approval-mode allow)")]
    auto_approve_mcp: bool,

    #[arg(long, help = "Deny remote MCP approval requests without prompting")]
    non_interactive: bool,

    #[arg(
        long,
        value_name = "PATH",
        help = "Workspace root exposed to workspace tools",
        conflicts_with = "environment"
    )]
    workspace: Option<PathBuf>,

    #[arg(
        long,
        conflicts_with = "environment",
        help = "Allow workspace_write, workspace_edit, workspace_move, and workspace_delete to modify files"
    )]
    allow_writes: bool,

    #[arg(
        long,
        value_name = "NAME",
        help = "Use a configured environment and its tool restrictions"
    )]
    environment: Option<String>,

    #[arg(
        long,
        short = 'q',
        conflicts_with = "verbose",
        help = "Hide progress messages"
    )]
    quiet: bool,

    #[arg(
        long,
        short = 'v',
        help = "Include tool arguments and full results in progress logs"
    )]
    verbose: bool,

    #[arg(
        long,
        help = "Print answers as Markdown without formatting them for the terminal"
    )]
    raw: bool,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    agent: AgentOptions,

    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    #[arg(long = "image", short = 'i', value_name = "PATH")]
    images: Vec<PathBuf>,

    #[arg(long = "audio", short = 'a', value_name = "PATH")]
    audio: Vec<PathBuf>,

    #[arg(long, help = "Print the result, response ID, and events as JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct ChatArgs {
    #[command(flatten)]
    agent: AgentOptions,
}

#[derive(Debug, Args)]
struct SessionArgs {
    path: PathBuf,
    #[arg(long, help = "Print the full saved conversation as JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct ToolsArgs {
    #[arg(long = "disable-tool", value_name = "NAME")]
    disabled_tools: Vec<String>,

    #[arg(
        long,
        value_name = "NAME",
        help = "Filter tools using a configured environment"
    )]
    environment: Option<String>,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[arg(long, value_name = "ADDRESS", help = "Override webhook bind address")]
    bind: Option<String>,

    #[arg(long, value_name = "PATH", help = "Override webhook path")]
    path: Option<String>,

    #[arg(long, help = "Run without ANO_WEBHOOK_SECRET (local development only)")]
    allow_unauthenticated: bool,
}

/// Parse the process arguments and run the selected command.
pub async fn run() -> Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let config = match &cli.config {
        Some(path) => AppConfig::load(path)?,
        None => AppConfig::load_or_default(DEFAULT_CONFIG_PATH)?,
    };
    if !config.has_user(&cli.user) {
        bail!(
            "unknown user '{}'; define it in [users] or use --user default",
            cli.user
        );
    }
    let registry = ToolRegistry::new();
    register_builtin_tools(&registry)?;

    match cli.command {
        Command::Session(args) => {
            let session = Session::inspect(args.path)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&session)?);
            } else {
                println!("Status: {:?}\nCompleted turns: {}\nHistory items: {}\nUser: {}\nEnvironment: {}",
                    session.status, session.completed_turns, session.history.len(), session.binding.user_id, session.binding.environment);
                if let Some(error) = session.last_error {
                    println!("Last error: {error}");
                }
                if !session.plan.steps.is_empty() {
                    println!("{}", output::format_plan(&session.plan));
                }
                println!(
                    "Usage: {} reported tokens, {} requests without usage\nCompactions: {}",
                    session.usage.total_tokens,
                    session.usage.unreported_requests,
                    session.compactions.len()
                );
            }
            Ok(())
        }
        Command::Tools(args) => list_tools(&config, &cli.user, &args, &registry),
        Command::Run(args) => run_agent(config, cli.user, args, registry).await,
        Command::Chat(args) => chat::run(config, cli.user, args.agent, registry).await,
        Command::Serve(args) => serve(config, args, registry).await,
        Command::Mcp(args) => mcp::run(&config, args).await,
    }
}

fn list_tools(
    config: &AppConfig,
    user_id: &str,
    args: &ToolsArgs,
    registry: &ToolRegistry,
) -> Result<()> {
    let mut policy = config.policy_for(user_id, &args.disabled_tools);
    if let Some(name) = &args.environment {
        let environment = config.environment_for(name)?;
        policy = policy.with_restrictions(
            environment.allowed_tools.as_deref(),
            &environment.disabled_tools,
        );
    }
    println!("Local tools:");
    if !policy.is_disabled(TASK_PLAN_NAME) {
        println!("  task_plan - Read/update this run's task plan (always available)");
    }
    for definition in registry.definitions(&policy) {
        println!("  {} - {}", definition.name, definition.description);
    }
    println!("\nMCP servers:");
    for server in &config.mcp_servers {
        if policy.is_mcp_server_disabled(&server.label) {
            println!("  {} (disabled)", server.label);
            continue;
        }
        let filtered = if server.transport == McpTransport::Responses {
            Some(
                server
                    .discoverable_tools(&policy)
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect::<Vec<_>>(),
            )
        } else {
            server.allowed_tools.as_ref().map(|names| {
                names
                    .iter()
                    .filter(|name| server.is_tool_allowed(&policy, name))
                    .cloned()
                    .collect::<Vec<_>>()
            })
        };
        let target = match server.transport {
            McpTransport::Responses => server
                .url
                .as_deref()
                .or(server.tunnel_id.as_deref())
                .unwrap_or("(invalid remote MCP configuration)")
                .to_string(),
            McpTransport::Stdio => format!(
                "stdio: {}",
                server.command.as_deref().unwrap_or("(missing command)")
            ),
            McpTransport::StreamableHttp => format!(
                "streamable_http: {}{}",
                server.url.as_deref().unwrap_or("(missing URL)"),
                if server.oauth { " (OAuth)" } else { "" }
            ),
        };
        println!("  {} ({:?}) -> {}", server.label, server.transport, target);
        if let Some(names) = filtered {
            if names.is_empty() {
                println!("    discoverable_tools: none (check catalog and policy)");
            } else {
                println!(
                    "    configured tools permitted by policy: {}",
                    names.join(", ")
                );
            }
        } else {
            println!("    tool list not fetched; user and environment policies apply at runtime");
        }
    }
    Ok(())
}

/// An agent wired to its adapters for one CLI invocation.
struct PreparedAgent {
    agent: Agent,
    context: ToolContext,
    /// Identifies the conversation's user, environment, and endpoint.
    binding: SessionBinding,
    session: Option<Session>,
    mcp: Arc<dyn McpGateway>,
}

fn prepare_agent(
    config: &AppConfig,
    user_id: &str,
    options: &AgentOptions,
    registry: ToolRegistry,
    ask_user: Arc<dyn ApprovalHandler>,
    stdin_is_terminal: bool,
) -> Result<PreparedAgent> {
    let profile = resolve_run_context(config, user_id, options)?;
    let client = Arc::new(OpenAiClient::from_api_settings(&config.api)?);
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
    let approval = profile.approval_handler(client.clone(), ask_user);
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
        .map(|path| Session::open(path, binding.clone(), options.recover_session))
        .transpose()?;
    let mcp: Arc<dyn McpGateway> = Arc::new(McpPool::new(config.mcp_servers.clone()));
    let mut agent = Agent::new(
        client,
        settings,
        Arc::clone(&mcp),
        registry,
        policy,
        approval,
    );
    if !options.quiet {
        let verbose = options.verbose;
        agent =
            agent.with_event_listener(Arc::new(move |event| output::print_event(event, verbose)));
    }
    Ok(PreparedAgent {
        agent,
        context,
        binding,
        session,
        mcp,
    })
}

async fn run_agent(
    config: AppConfig,
    user_id: String,
    args: RunArgs,
    registry: ToolRegistry,
) -> Result<()> {
    let stdin_is_terminal = std::io::stdin().is_terminal();
    // Validate options that need no network before reading a piped prompt.
    resolve_run_context(&config, &user_id, &args.agent)?;
    let prompt = match args.prompt {
        Some(prompt) => Some(prompt),
        // Only read a piped prompt; never block waiting on an interactive
        // terminal when the user supplied only --image or --audio.
        None if !stdin_is_terminal => read_stdin_prompt()?,
        None => None,
    };
    if prompt.is_none() && args.images.is_empty() && args.audio.is_empty() {
        anyhow::bail!("provide a prompt, --image, or --audio");
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
    )?;

    let mut input = Vec::new();
    if let Some(prompt) = prompt {
        input.push(InputPart::Text(prompt));
    }
    input.extend(args.images.into_iter().map(InputPart::Image));
    input.extend(args.audio.into_iter().map(InputPart::Audio));

    let request = RunRequest { input, context };
    let result = match &mut session {
        Some(session) => agent.run_in_session(request, session).await,
        None => agent.run(request).await,
    };
    // Stop stdio MCP server processes before exiting, even on failure.
    mcp.shutdown().await;
    let result = result?;
    let format = output::TextFormat::for_stdout(args.agent.raw);
    println!("{}", output::format_result(&result, args.json, format)?);
    Ok(())
}

fn resolve_run_context(
    config: &AppConfig,
    user_id: &str,
    args: &AgentOptions,
) -> Result<ExecutionProfile> {
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
    if let Some(limit) = args.max_total_tokens {
        settings.max_total_tokens = Some(limit);
    }
    if let Some(model) = &args.model {
        settings.model.clone_from(model);
    }
    if let Some(effort) = &args.reasoning_effort {
        settings.reasoning_effort = Some(effort.clone());
    }
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
    Ok(profile)
}

async fn serve(mut config: AppConfig, args: ServeArgs, registry: ToolRegistry) -> Result<()> {
    if let Some(bind) = args.bind {
        config.webhook.bind = bind;
    }
    if let Some(path) = args.path {
        config.webhook.path = path;
    }
    if args.allow_unauthenticated {
        config.webhook.allow_unauthenticated = true;
    }
    let client = OpenAiClient::from_api_settings(&config.api)?;
    let mcp: Arc<dyn McpGateway> = Arc::new(McpPool::new(config.mcp_servers.clone()));
    let served = webhook::serve(config, client, Arc::clone(&mcp), registry).await;
    // Close MCP connections after jobs have stopped, even on failure.
    mcp.shutdown().await;
    served
}

fn read_stdin_prompt() -> Result<Option<String>> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("failed to read prompt from stdin")?;
    let input = input.trim().to_string();
    Ok((!input.is_empty()).then_some(input))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        } = resolve_run_context(&config, "default", &args.agent).unwrap();
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
        assert_eq!(approval_mode, ApprovalMode::Deny);
    }

    #[test]
    fn cli_cannot_override_profile_permissions() {
        for extra in [
            vec!["--workspace", "."],
            vec!["--allow-writes"],
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
}
