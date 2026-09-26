//! Application layer: the agent use case and the ports it needs.
//!
//! Depends only on `domain`. External systems are reached through the traits
//! in [`ports`], which `infrastructure` and `interface` implement.

pub mod agent;
pub mod approval;
pub mod auto_approval;
pub mod input;
pub mod ports;
pub mod profile;
pub mod registry;
pub mod settings;
