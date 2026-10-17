//! Infrastructure layer: adapters for the OpenAI HTTP API, MCP connections,
//! session files, the built-in workspace tools, and the sources, state, and
//! notifiers of automation.

pub mod automation;
pub mod automation_store;
pub mod chat_completions;
pub mod chatgpt;
pub mod chronotope;
pub mod fallback;
pub(crate) mod fs;
#[cfg(unix)]
pub mod login_shell;
pub mod mcp;
pub mod mcp_oauth;
pub mod memory_store;
pub mod openai;
pub mod project;
pub mod session_store;
pub mod skills;
pub mod tools;
