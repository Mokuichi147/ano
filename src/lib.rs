//! An autonomous agent backed by the OpenAI Responses API.
//!
//! The crate separates a general agent from the harness that puts it to
//! work in a workspace, in a layered (clean) architecture. Dependencies
//! point inward only:
//!
//! ```text
//! interface ─> harness ─┬─> config ─────────┐
//!                       └─> infrastructure ─┴─> application ─> domain
//! ```
//!
//! - [`domain`]: plans, token usage, tool policies, conversation state.
//! - [`application`]: the agent: its run loop, which knows no particular
//!   tools, its extension points, and its ports (traits).
//! - [`infrastructure`]: OpenAI HTTP client, MCP connections, session files,
//!   built-in tools.
//! - [`harness`]: the agent at work in a workspace: its settings and
//!   instructions, the review gate, approval modes, and the assembly of
//!   agents from the config file.
//! - [`interface`]: the CLI and the webhook server.
//! - [`config`]: the configuration file, combining every layer's settings.
//!
//! The most common types are re-exported at the crate root.

pub mod application;
pub mod config;
pub mod domain;
pub mod harness;
pub mod infrastructure;
pub mod interface;

pub use application::{
    agent::{
        task_plan_definition, Agent, AgentEvent, AgentExtension, AgentResult, EventListener,
        ExtensionCall, ModelTarget, RunInfo, RunRequest, SubagentModels, SubagentSpec,
        DELEGATE_ROLE,
    },
    approval::{AlwaysApprove, DenyApproval},
    input::{build_user_input, InputPart},
    ports::{
        ApprovalHandler, ApprovalSource, ConversationStore, DirectMcpServer, DirectMcpTool,
        HistoryBackend, McpApprovalRequest, McpGateway, McpServerFailure, ResponsesApi,
    },
    registry::{ContextualToolHandler, ToolHandler, ToolRegistry},
    settings::AgentSettings,
};
pub use config::{
    AppConfig, ModelRequest, ModelSelection, PresetSettings, API_PROVIDER, DEFAULT_PRESET,
};
pub use domain::{
    compaction::CompactionRecord,
    environment::{CheckConfig, EnvironmentConfig},
    mcp::{McpApprovalMode, McpServerConfig, McpToolCatalog, McpTransport},
    plan::{PlanStep, RunOutcome, StepStatus, TaskPlan, TASK_PLAN_NAME},
    policy::UserPolicy,
    session::{ModelChoice, SessionBinding, SessionData, SessionStatus},
    tool::{ToolContext, ToolDefinition, DELEGATE_TASK_NAME, TOOL_SEARCH_NAME},
    usage::{ApiOperation, StopReason, UsageSummary},
};
pub use harness::{
    approval::ApprovalFactory,
    instructions::DEFAULT_INSTRUCTIONS,
    names::{REVIEW_CHANGES_NAME, WORKSPACE_EXEC_NAME},
    profile::ExecutionProfile,
    review::{ReviewGate, REVIEW_ROLE},
    settings::{AgentConfig, ModelRoles},
    Harness,
};
pub use infrastructure::{
    chronotope::{Chronotope, HistorySettings},
    mcp::McpPool,
    openai::{create_client, ApiAuth, ApiSettings, OpenAiClient, ProviderSettings},
    session_store::Session,
    tools::register_builtin_tools,
};
pub use interface::{
    cli::InteractiveApproval,
    webhook::{serve as serve_webhook, JobState, JobStatus, WebhookSettings},
};
