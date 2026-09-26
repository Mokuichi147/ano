//! Infrastructure layer: adapters for the OpenAI HTTP API, MCP connections,
//! session files, and the built-in workspace tools.

pub(crate) mod fs;
pub mod mcp;
pub mod memory_store;
pub mod openai;
pub mod project;
pub mod session_store;
pub mod tools;
