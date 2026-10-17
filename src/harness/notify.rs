//! `notify_user`: lets a run that automation started tell the user
//! something through the notifiers of its rule, when the run judges that
//! the user should know.

use crate::{
    application::{
        agent::{AgentExtension, ExtensionCall, RunInfo},
        automation::{NamedNotifier, Notification},
    },
    domain::{automation::Priority, text::cut, tool::ToolDefinition},
};
use async_trait::async_trait;
use futures::future::join_all;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

pub const NOTIFY_USER_NAME: &str = "notify_user";
/// Notifications one run may send, so a confused run cannot flood the user.
pub const MAX_NOTIFICATIONS_PER_RUN: usize = 5;
const MAX_TITLE_CHARS: usize = 200;
const MAX_MESSAGE_CHARS: usize = 3000;

/// Instructions added to the runs that automation starts.
pub const AUTOMATION_INSTRUCTIONS: &str = "This run was started automatically, and nobody is watching it: questions cannot be answered, so make reasonable choices within the task and report them. Your final answer is kept as the job's result.";

/// Added after `AUTOMATION_INSTRUCTIONS` when the run can use `notify_user`.
pub const NOTIFY_INSTRUCTIONS: &str = "Call notify_user when the user should know something now: a result the task says to report, a problem that needs them, or something unusual in the data. Do not notify routine progress or that nothing happened, unless the task asks for it; one notification at the end is usually enough.";

/// The `notify_user` tool of one run.
pub struct NotifyExtension {
    notifiers: Vec<NamedNotifier>,
    /// The rule that started the run, named in the notifications.
    rule: String,
    /// The page of the event that started the run.
    link: Option<String>,
    sent: AtomicUsize,
}

impl NotifyExtension {
    pub fn new(notifiers: Vec<NamedNotifier>, rule: &str, link: Option<String>) -> Self {
        Self {
            notifiers,
            rule: rule.to_string(),
            link,
            sent: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl AgentExtension for NotifyExtension {
    fn tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition::new(
            NOTIFY_USER_NAME,
            "Send the user a short notification (to their phone or chat, as configured). Use it when the user should know something now; do not use it for routine progress. The message may use Markdown.",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "A few words on what this is about."},
                    "message": {"type": "string", "description": "What happened and what, if anything, the user should do. A few sentences."},
                    "priority": {"type": "string", "enum": ["low", "normal", "high"], "description": "high only for what needs attention soon."}
                },
                "required": ["title", "message"],
                "additionalProperties": false
            }),
        )]
    }

    fn offers(&self, _name: &str, run: &RunInfo<'_>) -> bool {
        // Sub-agents report to the run, which decides what the user hears.
        run.depth == 0 && !self.notifiers.is_empty()
    }

    async fn call_tool(&self, _name: &str, arguments: &Value, _call: ExtensionCall<'_>) -> Value {
        let failure = |error: &str, message: &str| json!({"error": error, "tool": NOTIFY_USER_NAME, "message": message});
        let text = |field: &str| {
            arguments[field]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
        };
        let (Some(title), Some(message)) = (text("title"), text("message")) else {
            return failure("invalid_arguments", "title and message must not be empty.");
        };
        let priority = match arguments["priority"].as_str() {
            None => Priority::Normal,
            Some(word) => match Priority::parse(word) {
                Some(priority) => priority,
                None => {
                    return failure(
                        "invalid_arguments",
                        "priority must be low, normal, or high.",
                    )
                }
            },
        };
        if self.sent.fetch_add(1, Ordering::SeqCst) >= MAX_NOTIFICATIONS_PER_RUN {
            return failure(
                "limit_reached",
                &format!("this run already sent {MAX_NOTIFICATIONS_PER_RUN} notifications; put the rest in the final answer."),
            );
        }
        let notification = Notification {
            title: cut(title, MAX_TITLE_CHARS),
            message: cut(message, MAX_MESSAGE_CHARS),
            priority,
            url: self.link.clone(),
            rule: Some(self.rule.clone()),
        };
        let results = join_all(self.notifiers.iter().map(|notifier| {
            let notification = &notification;
            async move {
                (
                    notifier.name.as_str(),
                    notifier.notifier.send(notification).await,
                )
            }
        }))
        .await;
        let sent: Vec<&str> = results
            .iter()
            .filter(|(_, result)| result.is_ok())
            .map(|(name, _)| *name)
            .collect();
        let failed: Vec<Value> = results
            .iter()
            .filter_map(|(name, result)| {
                result
                    .as_ref()
                    .err()
                    .map(|error| json!({"notifier": name, "error": format!("{error:#}")}))
            })
            .collect();
        if sent.is_empty() {
            return json!({"error": "notification_failed", "tool": NOTIFY_USER_NAME, "failed": failed});
        }
        json!({"sent": sent, "failed": failed})
    }
}
