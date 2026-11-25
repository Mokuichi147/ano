//! Infrastructure layer: adapters for the OpenAI and Anthropic HTTP APIs, MCP connections,
//! session files, the built-in workspace tools, and the sources, state, and
//! notifiers of automation.

pub mod agenda;
pub mod anthropic;
pub mod automation;
pub mod automation_store;
pub mod character;
pub mod chat_completions;
pub mod chatgpt;
pub mod chronotope;
pub mod events;
pub mod fallback;
pub mod feed;
pub(crate) mod frontmatter;
pub(crate) mod fs;
#[cfg(unix)]
pub mod login_shell;
pub mod mcp;
pub mod mcp_oauth;
pub mod memory_store;
pub mod objectives;
pub mod openai;
pub mod project;
pub mod routines;
pub mod session_store;
pub mod skills;
pub mod spaces;
pub mod tools;
pub mod user_memory;
