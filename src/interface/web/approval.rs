//! Approval requests answered in the browser.

use super::events::{EventLog, UiEvent};
use crate::{
    application::ports::{ApprovalHandler, ApprovalSource, McpApprovalRequest},
    interface::shorten,
};
use anyhow::Result;
use async_trait::async_trait;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;
use uuid::Uuid;

/// Longer arguments are shown cut to a preview, which the page marks.
const MAX_ARGUMENT_CHARS: usize = 20_000;

/// Requests of a session that wait for the user's answer.
#[derive(Default)]
pub(super) struct PendingApprovals {
    waiting: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl PendingApprovals {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<bool>>> {
        self.waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Answer request `id`. False when no such request waits, e.g. when it
    /// was already answered or its turn was cancelled.
    pub(super) fn answer(&self, id: &str, approved: bool) -> bool {
        self.lock()
            .remove(id)
            .is_some_and(|reply| reply.send(approved).is_ok())
    }
}

/// Shows each request in the session's page and waits for the answer. A
/// request whose turn is cancelled counts as denied.
pub(super) struct WebApproval {
    pub(super) log: Arc<EventLog>,
    pub(super) pending: Arc<PendingApprovals>,
}

/// Withdraws a request whose wait ended without an answer.
struct Waiting<'a> {
    approval: &'a WebApproval,
    id: String,
    answered: bool,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if !self.answered {
            self.approval.pending.lock().remove(&self.id);
            self.approval.log.push(UiEvent::ApprovalResolved {
                id: self.id.clone(),
                approved: false,
            });
        }
    }
}

#[async_trait]
impl ApprovalHandler for WebApproval {
    async fn approve(&self, request: McpApprovalRequest) -> Result<bool> {
        let id = Uuid::new_v4().to_string();
        let (reply, answer) = oneshot::channel();
        self.pending.lock().insert(id.clone(), reply);
        let mut arguments = request.arguments.clone();
        shorten(&mut arguments, MAX_ARGUMENT_CHARS);
        self.log.push(UiEvent::ApprovalRequested {
            id: id.clone(),
            mcp: request.source == ApprovalSource::Mcp,
            target: request.target(),
            arguments,
            review: request.review.clone(),
        });
        let mut waiting = Waiting {
            approval: self,
            id,
            answered: false,
        };
        let approved = answer.await.unwrap_or(false);
        waiting.answered = true;
        self.log.push(UiEvent::ApprovalResolved {
            id: waiting.id.clone(),
            approved,
        });
        Ok(approved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn request() -> McpApprovalRequest {
        McpApprovalRequest {
            approval_request_id: "call".into(),
            source: ApprovalSource::LocalTool,
            server_label: String::new(),
            tool_name: "workspace_exec".into(),
            arguments: json!({"command": "ls"}),
            tool_description: None,
            user_request: "list".into(),
            review: None,
        }
    }

    fn requested_id(log: &EventLog) -> String {
        let (events, _) = log.subscribe(0);
        let value: Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
        assert_eq!(value["type"], "approval_requested");
        assert_eq!(value["target"], "workspace_exec");
        value["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn the_answer_from_the_page_decides_and_cancelled_requests_are_denied() {
        let log = Arc::new(EventLog::default());
        let pending = Arc::new(PendingApprovals::default());
        let approval = Arc::new(WebApproval {
            log: Arc::clone(&log),
            pending: Arc::clone(&pending),
        });

        let asking = tokio::spawn({
            let approval = Arc::clone(&approval);
            async move { approval.approve(request()).await.unwrap() }
        });
        while log.subscribe(0).0.is_empty() {
            tokio::task::yield_now().await;
        }
        let id = requested_id(&log);
        assert!(!pending.answer("other", true));
        assert!(pending.answer(&id, true));
        assert!(asking.await.unwrap());
        assert!(!pending.answer(&id, false));

        let asking = tokio::spawn({
            let approval = Arc::clone(&approval);
            async move { approval.approve(request()).await.unwrap() }
        });
        while log.subscribe(0).0.len() < 3 {
            tokio::task::yield_now().await;
        }
        let id = requested_id(&log);
        asking.abort();
        assert!(asking.await.is_err());
        assert!(!pending.answer(&id, true));
        let (events, _) = log.subscribe(0);
        let last: Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
        assert_eq!(
            last,
            json!({"type": "approval_resolved", "id": id, "approved": false})
        );
    }

    #[tokio::test]
    async fn long_arguments_are_shown_cut() {
        let log = Arc::new(EventLog::default());
        let approval = WebApproval {
            log: Arc::clone(&log),
            pending: Arc::default(),
        };
        let mut long = request();
        long.arguments = json!({"command": "x".repeat(MAX_ARGUMENT_CHARS)});
        let asking = approval.approve(long);
        tokio::pin!(asking);
        assert!(futures::poll!(&mut asking).is_pending());
        let (events, _) = log.subscribe(0);
        let value: Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(value["arguments"]["truncated"], true);
        assert_eq!(
            value["arguments"]["preview"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            MAX_ARGUMENT_CHARS
        );
    }
}
