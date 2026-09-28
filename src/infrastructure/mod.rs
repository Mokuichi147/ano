//! Infrastructure layer: adapters for the OpenAI HTTP API, MCP connections,
//! session files, and the built-in workspace tools.

pub mod chatgpt;
pub mod chronotope;
pub mod fallback;
pub(crate) mod fs;
pub mod mcp;
pub mod mcp_oauth;
pub mod memory_store;
pub mod openai;
pub mod project;
pub mod session_store;
pub mod skills;
pub mod tools;
