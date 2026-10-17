//! Approval requests answered in the editor, with `session/request_permission`.

use super::{updates::Updates, Notifier};
use crate::application::ports::{ApprovalHandler, McpApprovalRequest};
use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    SessionUpdate, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

const ALLOW: &str = "allow";
const REJECT: &str = "reject";

/// Asks the editor about each request and waits for the answer. A request
/// the editor cancels, or cannot answer, counts as denied.
pub(super) struct AcpApproval {
    pub(super) notifier: Notifier,
    pub(super) updates: Arc<Mutex<Updates>>,
}

#[async_trait]
impl ApprovalHandler for AcpApproval {
    async fn approve(&self, request: McpApprovalRequest) -> Result<bool> {
        let (id, announce) = self
            .updates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .approval_target(
                request.call_id.as_deref(),
                &request.approval_request_id,
                &request.target(),
                &request.arguments,
            );
        let announced = announce.is_some();
        if let Some(update) = announce {
            self.notifier.update(update);
        }
        let options = vec![
            PermissionOption::new(ALLOW, "Allow", PermissionOptionKind::AllowOnce),
            PermissionOption::new(REJECT, "Reject", PermissionOptionKind::RejectOnce),
        ];
        let asked = RequestPermissionRequest::new(
            self.notifier.session_id.clone(),
            ToolCallUpdate::new(id.clone(), ToolCallUpdateFields::new()),
            options,
        );
        let answer = self
            .notifier
            .connection
            .send_request(asked)
            .block_task()
            .await;
        let approved = match answer {
            Ok(response) => matches!(
                response.outcome,
                RequestPermissionOutcome::Selected(selected) if selected.option_id.to_string() == ALLOW
            ),
            Err(error) => {
                eprintln!("warning: the editor did not answer an approval request: {error}");
                false
            }
        };
        // A call the editor saw only for this request ends with its answer:
        // its result, if any, comes from the provider.
        if announced {
            let status = if approved {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            };
            self.notifier
                .update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    id,
                    ToolCallUpdateFields::new().status(status),
                )));
        }
        Ok(approved)
    }
}
