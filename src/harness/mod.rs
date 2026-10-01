//! The coding harness: what turns the general run loop of `application`
//! into an agent that works in a workspace. It names the built-in tools its
//! rules refer to and adds the runtime tools and rules of that work to the
//! agent through `AgentExtension`.

pub mod instructions;
pub mod names;
pub mod profile;
pub mod review;
pub mod settings;
