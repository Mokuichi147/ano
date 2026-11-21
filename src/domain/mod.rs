//! Domain layer: the rules of plans, token budgets, tool policies,
//! conversation state, and compaction.
//!
//! This layer performs no network or process I/O and depends on no other
//! layer. Conversation items use the Responses API JSON format as-is, so
//! history, compaction, and usage are expressed in that format.

pub mod agenda;
pub mod approval;
pub mod automation;
pub mod character;
pub mod compaction;
pub mod environment;
pub mod feed;
pub mod github;
pub mod history;
pub mod mcp;
pub mod objective;
pub mod plan;
pub mod policy;
pub mod provider;
pub mod routine;
pub mod session;
pub mod skill;
pub mod space;
pub mod text;
pub mod tool;
pub mod usage;
pub mod user_memory;
