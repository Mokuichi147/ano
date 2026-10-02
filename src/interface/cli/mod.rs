//! The `ano` command line. This module is also the composition root of the
//! binary: it builds the infrastructure adapters and wires them into the
//! application layer.

mod approval;
mod auth;
mod chat;
mod history;
mod mcp;
mod model;
mod output;
mod preset;
mod prompt;
mod provider;
mod run;
mod skills;
mod tools;

pub use approval::InteractiveApproval;

use crate::{
    application::{ports::McpGateway, registry::ToolRegistry},
    config::{default_config_path, user_env_path, AppConfig},
    domain::approval::ApprovalMode,
    harness::{models::connect_provider, Harness},
    infrastructure::{mcp::McpPool, session_store::Session, tools::register_builtin_tools},
    interface::{web, webhook},
};
use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::{path::PathBuf, sync::Arc};

#[derive(Debug, Parser)]
#[command(
    name = "ano",
    version,
    about = "Autonomous Rust agent powered by the OpenAI Responses API"
)]
struct Cli {
    /// Config file. Defaults to ./config.toml when present, otherwise
    /// config.toml in the OS config directory (macOS: ~/Library/Application
    /// Support/ano, Linux: ~/.config/ano, Windows: %APPDATA%\ano\config); an
    /// explicitly given path must exist.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[arg(long, global = true, default_value = "default")]
    user: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// ChatGPT サブスクリプションの認証情報を管理する。
    Auth(auth::AuthArgs),
    /// Run one task and print the result.
    Run(RunArgs),
    /// Talk with the agent over several turns in one conversation.
    Chat(ChatArgs),
    Tools(ToolsArgs),
    Serve(ServeArgs),
    /// Talk with the agent in the browser, in sessions with their own
    /// working folder, permissions, and model.
    Web(WebArgs),
    /// Inspect saved conversation state without contacting the model.
    Session(SessionArgs),
    /// Manage the authorization and the enabled tools of MCP servers.
    Mcp(mcp::McpArgs),
    /// Add, change, and remove providers, and enable or disable them.
    Provider(provider::ProviderArgs),
    /// Manage presets: named sets of a provider, a model, and a reasoning
    /// effort, and the presets of sub-agents and the approval reviewer.
    Preset(preset::PresetArgs),
    /// List the models of providers, and enable or disable them.
    Model(model::ModelArgs),
    /// 原文履歴の同期状態を調べ、chronotope へ再送・検索する。
    History(history::HistoryArgs),
    /// List the saved skills, or show one.
    Skills(skills::SkillsArgs),
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
        help = "Compact history after this many JSON bytes (see agent.compaction)"
    )]
    compact_threshold_bytes: Option<usize>,

    #[arg(
        long,
        help = "Stop after a response reaches this run's observed token budget (soft limit)"
    )]
    max_total_tokens: Option<u64>,

    #[arg(
        long,
        value_name = "N",
        help = "Responses requests this run may make (overrides agent.max_tool_rounds); the last one is reserved for a tool-free report"
    )]
    max_tool_rounds: Option<usize>,

    #[arg(long = "disable-tool", value_name = "NAME")]
    disabled_tools: Vec<String>,

    #[arg(
        long,
        value_name = "NAME",
        help = "Use a preset from [presets] (provider, model, and effort); 'default' is [agent]. --provider, --model, and --reasoning-effort apply over it"
    )]
    preset: Option<String>,

    #[arg(long, help = "Model to use (on the selected provider)")]
    model: Option<String>,

    #[arg(
        long,
        value_name = "NAME",
        help = "Connect to a provider from [providers] ('api' is [api]); defaults to [agent].provider"
    )]
    provider: Option<String>,

    #[arg(
        long,
        value_name = "LEVEL",
        help = "Reasoning effort: none, minimal, low, medium, high, xhigh, max, or ultra (which ones work depends on the model)"
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
        conflicts_with = "environment",
        help = "Allow workspace_exec to run shell commands in the workspace (each command needs approval)"
    )]
    allow_exec: bool,

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
        help = "Keep every step in the progress log, with tool arguments and full results"
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

    #[arg(
        long,
        value_name = "TEXT",
        help = "Set the goal (the end state, with any conditions it must meet) and keep working until it is verified; the prompt may then be omitted"
    )]
    goal: Option<String>,
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

#[derive(Debug, Args)]
struct WebArgs {
    #[arg(
        long,
        value_name = "ADDRESS",
        default_value = "127.0.0.1:8787",
        help = "Address and port to listen on; other than loopback (such as 0.0.0.0:8787), other machines can open the UI with the token, over plain HTTP"
    )]
    bind: String,

    #[arg(
        long,
        help = "Let other machines use the UI without the token (this machine never needs it); anyone who can reach the server can then run the agent"
    )]
    no_auth: bool,

    #[arg(
        long,
        value_name = "PATH",
        help = "Working folder that new sessions start with (defaults to the current directory)"
    )]
    workspace: Option<PathBuf>,
}

/// Parse the process arguments and run the selected command.
pub async fn run() -> Result<()> {
    // Neither file overrides variables that are already set, so the process
    // environment comes first, then the current directory's .env.
    dotenvy::dotenv().ok();
    if let Some(path) = user_env_path().filter(|path| path.exists()) {
        if let Err(error) = dotenvy::from_path(&path) {
            eprintln!("warning: failed to read {}: {error}", path.display());
        }
    }
    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(default_config_path);
    let config = match &cli.config {
        Some(path) => AppConfig::load(path)?,
        None => AppConfig::load_or_default(&config_path)?,
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
        Command::Auth(args) => auth::run(&config.api, args).await,
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
                if !session.plan.steps.is_empty() || session.plan.goal.is_some() {
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
        // `run`, `chat`, and `serve` set up the raw history once the rest of
        // the command has been checked; other commands must work without its
        // token.
        Command::Tools(args) => {
            let harness = Harness::with_registry(registry, &config)?;
            tools::list_tools(&config, &cli.user, &args, &harness.registry)
        }
        Command::Run(args) => run::run_agent(config, cli.user, args, registry).await,
        Command::Chat(args) => chat::run(config, cli.user, args.agent, registry).await,
        Command::Serve(args) => serve(config, args, registry).await,
        Command::Web(args) => web(config, cli.user, args, registry).await,
        Command::Mcp(args) => mcp::run(&config, &config_path, &cli.user, args).await,
        Command::Provider(args) => provider::run(&config, &config_path, args).await,
        Command::Preset(args) => preset::run(&config, &config_path, args).await,
        Command::Model(args) => model::run(&config, &config_path, args).await,
        Command::History(args) => history::run(&config, &cli.user, args).await,
        Command::Skills(args) => skills::run(&config, &cli.user, args),
    }
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
    let client = connect_provider(&config, config.default_provider())?;
    let mcp: Arc<dyn McpGateway> = Arc::new(McpPool::new(config.mcp_servers.clone()));
    let served = webhook::serve(config, client, Arc::clone(&mcp), registry).await;
    // Close MCP connections after jobs have stopped, even on failure.
    mcp.shutdown().await;
    served
}

async fn web(config: AppConfig, user: String, args: WebArgs, registry: ToolRegistry) -> Result<()> {
    let workspace = match args.workspace {
        Some(path) => path,
        None => std::env::current_dir().context("failed to determine current directory")?,
    };
    let options = web::WebOptions {
        bind: args.bind,
        workspace,
        user,
        token: std::env::var(web::TOKEN_ENV).ok(),
        authenticate: !args.no_auth,
    };
    let mcp: Arc<dyn McpGateway> = Arc::new(McpPool::new(config.mcp_servers.clone()));
    let served = web::serve(config, Arc::clone(&mcp), registry, options).await;
    // Close MCP connections after the sessions have stopped, even on failure.
    mcp.shutdown().await;
    served
}
