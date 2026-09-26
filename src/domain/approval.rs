//! How approval requests for MCP tool calls are answered.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Ask the user. Without a terminal, requests are denied.
    #[default]
    Ask,
    /// A reviewer model allows low-risk calls within the user's request,
    /// denies clearly unsafe ones, and asks the user about the rest.
    Auto,
    /// Approve every request.
    Allow,
    /// Deny every request.
    Deny,
}

impl ApprovalMode {
    pub const NAMES: &'static [&'static str] = &["ask", "auto", "allow", "deny"];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

impl fmt::Display for ApprovalMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ApprovalMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "ask" => Ok(Self::Ask),
            "auto" => Ok(Self::Auto),
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            _ => Err(format!(
                "approval mode must be one of {}",
                Self::NAMES.join(", ")
            )),
        }
    }
}
