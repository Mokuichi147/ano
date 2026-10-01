//! The coding harness: what turns the general run loop of `application`
//! into an agent that works in a workspace. It reads the `[agent]` settings
//! and the instructions for that work, names the built-in tools its rules
//! refer to, adds the runtime tools and rules of the work to the agent
//! through `AgentExtension`, and assembles agents from the config file
//! (`Harness`) for the command line and the webhook server.

mod assembly;

pub mod approval;
pub mod auto_approval;
pub mod instructions;
pub mod models;
pub mod profile;
pub mod review;
pub mod settings;

pub use assembly::Harness;
