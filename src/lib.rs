//! An autonomous agent backed by the OpenAI Responses API.
//!
//! The crate follows a layered (clean) architecture. Dependencies point
//! inward only:
//!
//! ```text
//! interface ─┐
//!            ├─> application ─> domain
//! infrastructure ┘
//! ```
//!
//! - [`domain`]: plans, token usage, tool policies, conversation state.
//! - [`application`]: the agent run loop and its ports (traits).
//! - [`infrastructure`]: OpenAI HTTP client, MCP connections, session files,
//!   built-in tools.
//! - [`interface`]: the CLI (also the binary's composition root) and the
//!   webhook server.
//! - [`config`]: the configuration file, combining every layer's settings.
//!
//! The most common types are re-exported at the crate root.

pub mod application;
pub mod config;
pub mod domain;
pub mod infrastructure;
pub mod interface;

pub use application::{
    agent::{task_plan_definition, Agent, AgentEvent, AgentResult, EventListener, RunRequest},
    approval::{AlwaysApprove, DenyApproval},
    input::{build_user_input, InputPart},
    ports::{
        ApprovalHandler, ApprovalSource, ConversationStore, DirectMcpServer, DirectMcpTool,
        HistoryBackend, McpApprovalRequest, McpGateway, ResponsesApi,
    },
    profile::ExecutionProfile,
    registry::{ContextualToolHandler, ToolHandler, ToolRegistry},
    settings::AgentSettings,
};
pub use config::AppConfig;
pub use domain::{
    compaction::CompactionRecord,
    environment::{CheckConfig, EnvironmentConfig},
    mcp::{McpApprovalMode, McpServerConfig, McpToolCatalog, McpTransport},
    plan::{PlanStep, RunOutcome, StepStatus, TaskPlan, TASK_PLAN_NAME},
    policy::UserPolicy,
    session::{SessionBinding, SessionData, SessionStatus},
    tool::{
        ToolContext, ToolDefinition, DELEGATE_TASK_NAME, TOOL_SEARCH_NAME, WORKSPACE_EXEC_NAME,
    },
    usage::{ApiOperation, StopReason, UsageSummary},
};
pub use infrastructure::{
    chronotope::{Chronotope, HistorySettings},
    mcp::McpPool,
    openai::{ApiSettings, OpenAiClient},
    session_store::Session,
    tools::register_builtin_tools,
};
pub use interface::{
    cli::InteractiveApproval,
    webhook::{serve as serve_webhook, JobState, JobStatus, WebhookSettings},
};
