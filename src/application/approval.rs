//! Non-interactive approval policies for MCP tool calls.

use crate::application::ports::{ApprovalHandler, McpApprovalRequest};
use anyhow::Result;
use async_trait::async_trait;

pub struct AlwaysApprove;

#[async_trait]
impl ApprovalHandler for AlwaysApprove {
    async fn approve(&self, _request: McpApprovalRequest) -> Result<bool> {
        Ok(true)
    }
}

pub struct DenyApproval;

#[async_trait]
impl ApprovalHandler for DenyApproval {
    async fn approve(&self, _request: McpApprovalRequest) -> Result<bool> {
        Ok(false)
    }
}
