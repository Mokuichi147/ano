//! Interface layer: the command line, the webhook server, and the web UI,
//! which build their agents with the harness.

pub mod cli;
pub mod web;
pub mod webhook;

use crate::application::agent::AgentEvent;
use serde_json::{json, Value};

/// `event` as JSON for a client, with every field longer than `max_chars`
/// (tool arguments and outputs) cut to a preview.
pub(crate) fn summarize_event(event: &AgentEvent, max_chars: usize) -> Value {
    let mut value = serde_json::to_value(event).unwrap_or_else(|_| json!({"type": "unknown"}));
    if let Some(fields) = value.as_object_mut() {
        for field in fields.values_mut() {
            shorten(field, max_chars);
        }
    }
    value
}

/// Replace `value` with `{"truncated": true, "preview": ...}`, the start of
/// its JSON, when that is longer than `max_chars`.
pub(crate) fn shorten(value: &mut Value, max_chars: usize) {
    let text = value.to_string();
    if text.chars().count() > max_chars {
        *value = json!({
            "truncated": true,
            "preview": text.chars().take(max_chars).collect::<String>(),
        });
    }
}
