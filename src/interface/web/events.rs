//! What a session shows in the browser: a numbered log of its events, which
//! a page that connects or reconnects reads from where it left off, and the
//! text of the message being generated, which is sent as it grows but not
//! kept in the log.

use super::markdown;
use crate::{
    application::{agent::AgentEvent, ports::ResponseDelta},
    domain::{
        plan::{RunOutcome, TaskPlan},
        usage::{StopReason, UsageSummary},
    },
    interface::summarize_event,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

/// Events kept per session for pages that connect later.
const MAX_LOGGED_EVENTS: usize = 5000;
/// Live updates a slow page may fall behind by before it reconnects.
const BROADCAST_CAPACITY: usize = 1024;
/// Longer fields of agent events (tool arguments and outputs) are cut.
const MAX_EVENT_FIELD_CHARS: usize = 4000;

/// An event of a session, as the page receives it.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum UiEvent {
    UserMessage {
        text: String,
    },
    TurnStarted,
    /// What the model thought before it went on (its reasoning text, when
    /// the endpoint streams it).
    Thinking {
        text: String,
    },
    /// A complete message of the model, with its Markdown as HTML.
    Message {
        text: String,
        html: String,
    },
    /// An event of the run loop, with long fields cut to a preview.
    Agent {
        event: Value,
    },
    ApprovalRequested {
        id: String,
        /// An MCP call rather than a local tool.
        mcp: bool,
        target: String,
        arguments: Value,
    },
    ApprovalResolved {
        id: String,
        approved: bool,
    },
    TurnFinished {
        #[serde(skip_serializing_if = "Option::is_none")]
        outcome: Option<RunOutcome>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
        /// The conversation's usage so far.
        usage: UsageSummary,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        cancelled: bool,
    },
    /// The session ended; no more events follow.
    Closed,
}

impl UiEvent {
    pub(super) fn message(text: String) -> Self {
        let html = markdown::render(&text);
        Self::Message { text, html }
    }
}

/// One server-sent event: numbered when it is kept in the log, so the page
/// can resume after it.
#[derive(Debug, Clone)]
pub(super) struct Outgoing {
    pub(super) id: Option<u64>,
    pub(super) data: Arc<str>,
}

/// What the page shows outside the timeline.
#[derive(Debug, Clone, Default, Serialize)]
pub(super) struct Progress {
    pub(super) running: bool,
    pub(super) plan: Option<TaskPlan>,
    pub(super) usage: UsageSummary,
}

#[derive(Default)]
struct LogState {
    next_id: u64,
    events: VecDeque<Outgoing>,
    /// The message being generated.
    partial: String,
    /// The reasoning being streamed.
    reasoning: String,
    /// Reasoning was streamed since the last reasoning summary, which then
    /// repeats it.
    reasoning_streamed: bool,
    progress: Progress,
}

pub(super) struct EventLog {
    state: Mutex<LogState>,
    sender: broadcast::Sender<Outgoing>,
}

impl Default for EventLog {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            sender: broadcast::channel(BROADCAST_CAPACITY).0,
        }
    }
}

impl EventLog {
    fn lock(&self) -> std::sync::MutexGuard<'_, LogState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn push(&self, event: UiEvent) {
        let mut state = self.lock();
        self.push_locked(&mut state, &event);
    }

    fn push_locked(&self, state: &mut LogState, event: &UiEvent) {
        // Reasoning ends when anything else happens, and comes before it.
        if !matches!(event, UiEvent::Thinking { .. }) {
            self.flush_reasoning(state);
        }
        state.next_id += 1;
        let outgoing = Outgoing {
            id: Some(state.next_id),
            data: serialize(event),
        };
        state.events.push_back(outgoing.clone());
        if state.events.len() > MAX_LOGGED_EVENTS {
            state.events.pop_front();
        }
        // Nobody may be listening.
        self.sender.send(outgoing).ok();
    }

    fn send_transient(&self, data: Value) {
        self.sender
            .send(Outgoing {
                id: None,
                data: data.to_string().into(),
            })
            .ok();
    }

    /// Text the model streams. A finished message joins the log.
    pub(super) fn text_delta(&self, delta: ResponseDelta<'_>) {
        let mut state = self.lock();
        match delta {
            ResponseDelta::Text(text) => {
                self.flush_reasoning(&mut state);
                state.partial.push_str(text);
                self.send_transient(json!({"type": "delta", "text": text}));
            }
            ResponseDelta::MessageDone => self.flush_locked(&mut state),
            ResponseDelta::Reasoning(text) => {
                state.reasoning.push_str(text);
                self.send_transient(json!({"type": "reasoning", "text": text}))
            }
        }
    }

    /// Keep the streamed reasoning in the log.
    fn flush_reasoning(&self, state: &mut LogState) {
        let text = std::mem::take(&mut state.reasoning);
        if !text.trim().is_empty() {
            state.reasoning_streamed = true;
            self.push_locked(state, &UiEvent::Thinking { text });
        }
    }

    /// Keep a message that was cut off, e.g. by cancellation, in the log.
    pub(super) fn flush_partial(&self) {
        let mut state = self.lock();
        self.flush_locked(&mut state);
    }

    fn flush_locked(&self, state: &mut LogState) {
        let text = std::mem::take(&mut state.partial);
        if !text.trim().is_empty() {
            self.push_locked(state, &UiEvent::message(text));
        }
    }

    /// An event of the run loop.
    pub(super) fn record(&self, event: &AgentEvent) {
        let mut state = self.lock();
        match event {
            // Streamed messages are already in the log.
            AgentEvent::AssistantProgress { streamed: true, .. } => return,
            AgentEvent::AssistantProgress { text, .. } => {
                self.push_locked(&mut state, &UiEvent::message(text.clone()));
                return;
            }
            // The summary of reasoning that was not streamed is shown as
            // thinking; streamed reasoning already is.
            AgentEvent::ReasoningSummary { text, .. } => {
                self.flush_reasoning(&mut state);
                if !std::mem::take(&mut state.reasoning_streamed) {
                    self.push_locked(&mut state, &UiEvent::Thinking { text: text.clone() });
                }
                return;
            }
            AgentEvent::PlanUpdated { plan, .. } => state.progress.plan = Some(plan.clone()),
            _ => {}
        }
        let event = UiEvent::Agent {
            event: summarize_event(event, MAX_EVENT_FIELD_CHARS),
        };
        self.push_locked(&mut state, &event);
    }

    pub(super) fn start_turn(&self, text: String) {
        let mut state = self.lock();
        state.progress.running = true;
        state.reasoning_streamed = false;
        self.push_locked(&mut state, &UiEvent::UserMessage { text });
        self.push_locked(&mut state, &UiEvent::TurnStarted);
    }

    /// End the turn with `finished` (a `TurnFinished`), taking the plan and
    /// usage from the conversation.
    pub(super) fn finish_turn(&self, finished: UiEvent, plan: &TaskPlan, usage: &UsageSummary) {
        let mut state = self.lock();
        self.flush_locked(&mut state);
        state.progress.running = false;
        state.progress.plan = Some(plan.clone());
        state.progress.usage = usage.clone();
        self.push_locked(&mut state, &finished);
    }

    pub(super) fn progress(&self) -> Progress {
        self.lock().progress.clone()
    }

    /// The logged events after `after`, the message being generated, and a
    /// receiver of what follows, with nothing lost or repeated between them.
    /// When events after `after` were already dropped from the log, the
    /// backlog starts with a `reset`: the page rebuilds from the events kept,
    /// and `truncated` tells it that older ones are missing.
    pub(super) fn subscribe(&self, after: u64) -> (Vec<Outgoing>, broadcast::Receiver<Outgoing>) {
        let state = self.lock();
        let first = state.events.front().and_then(|event| event.id);
        let mut backlog = Vec::new();
        if first.is_some_and(|first| first > after + 1) {
            backlog.push(Outgoing {
                id: None,
                data: json!({"type": "reset", "truncated": true})
                    .to_string()
                    .into(),
            });
        }
        backlog.extend(
            state
                .events
                .iter()
                .filter(|event| event.id.is_some_and(|id| id > after))
                .cloned(),
        );
        if !state.reasoning.is_empty() {
            backlog.push(Outgoing {
                id: None,
                data: json!({"type": "reasoning_partial", "text": state.reasoning})
                    .to_string()
                    .into(),
            });
        }
        if !state.partial.is_empty() {
            backlog.push(Outgoing {
                id: None,
                data: json!({"type": "partial", "text": state.partial})
                    .to_string()
                    .into(),
            });
        }
        (backlog, self.sender.subscribe())
    }
}

fn serialize(event: &UiEvent) -> Arc<str> {
    serde_json::to_string(event)
        .unwrap_or_else(|_| r#"{"type":"unknown"}"#.to_string())
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(events: &[Outgoing]) -> Vec<String> {
        events
            .iter()
            .map(|event| {
                let value: Value = serde_json::from_str(&event.data).unwrap();
                value["type"].as_str().unwrap().to_string()
            })
            .collect()
    }

    #[test]
    fn reconnecting_pages_resume_after_their_last_event_and_see_the_partial_message() {
        let log = EventLog::default();
        log.start_turn("hello".into());
        log.text_delta(ResponseDelta::Text("Hi"));
        let (backlog, _) = log.subscribe(0);
        assert_eq!(types(&backlog), ["user_message", "turn_started", "partial"]);
        assert_eq!(backlog[1].id, Some(2));

        let (_, mut live) = log.subscribe(2);
        log.text_delta(ResponseDelta::Text(" there"));
        log.text_delta(ResponseDelta::MessageDone);
        let delta = live.try_recv().unwrap();
        assert_eq!(delta.id, None);
        let message = live.try_recv().unwrap();
        let value: Value = serde_json::from_str(&message.data).unwrap();
        assert_eq!(value["text"], "Hi there");
        assert_eq!(value["html"], "<p>Hi there</p>\n");

        let (backlog, _) = log.subscribe(2);
        assert_eq!(types(&backlog), ["message"]);
        assert_eq!(backlog[0].id, Some(3));
    }

    #[test]
    fn streamed_progress_is_not_repeated_and_cut_off_text_is_kept() {
        let log = EventLog::default();
        log.record(&AgentEvent::AssistantProgress {
            round: 1,
            text: "shown".into(),
            streamed: true,
        });
        log.record(&AgentEvent::AssistantProgress {
            round: 1,
            text: "not streamed".into(),
            streamed: false,
        });
        log.text_delta(ResponseDelta::Text("cut"));
        log.finish_turn(
            UiEvent::TurnFinished {
                outcome: None,
                stop_reason: None,
                usage: UsageSummary::default(),
                error: None,
                cancelled: true,
            },
            &TaskPlan::default(),
            &UsageSummary::default(),
        );
        let (backlog, _) = log.subscribe(0);
        assert_eq!(types(&backlog), ["message", "message", "turn_finished"]);
        assert!(!log.progress().running);
    }

    #[test]
    fn pages_behind_the_kept_events_are_told_to_rebuild() {
        let log = EventLog::default();
        for _ in 0..MAX_LOGGED_EVENTS + 2 {
            log.push(UiEvent::TurnStarted);
        }
        let (backlog, _) = log.subscribe(1);
        assert_eq!(backlog.len(), MAX_LOGGED_EVENTS + 1);
        let reset: Value = serde_json::from_str(&backlog[0].data).unwrap();
        assert_eq!(reset, json!({"type": "reset", "truncated": true}));
        assert_eq!(backlog[1].id, Some(3));
        // A page that kept up resumes without a reset.
        let (backlog, _) = log.subscribe(2);
        assert_eq!(backlog.len(), MAX_LOGGED_EVENTS);
        assert_eq!(backlog[0].id, Some(3));
    }

    #[test]
    fn streamed_reasoning_is_kept_before_what_follows_it() {
        let log = EventLog::default();
        log.start_turn("hello".into());
        log.text_delta(ResponseDelta::Reasoning("Let me "));
        log.text_delta(ResponseDelta::Reasoning("check."));
        // A page that connects now gets the reasoning so far.
        let (backlog, _) = log.subscribe(2);
        let partial: Value = serde_json::from_str(&backlog[0].data).unwrap();
        assert_eq!(
            partial,
            json!({"type": "reasoning_partial", "text": "Let me check."})
        );

        log.record(&AgentEvent::ToolSearch {
            round: 1,
            query: "x".into(),
            results: Vec::new(),
        });
        log.text_delta(ResponseDelta::Reasoning("Now answer."));
        log.text_delta(ResponseDelta::Text("Done"));
        log.text_delta(ResponseDelta::MessageDone);
        let (backlog, _) = log.subscribe(0);
        let events: Vec<Value> = backlog
            .iter()
            .map(|event| serde_json::from_str(&event.data).unwrap())
            .collect();
        let types: Vec<&str> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            [
                "user_message",
                "turn_started",
                "thinking",
                "agent",
                "thinking",
                "message"
            ]
        );
        assert_eq!(events[2]["text"], "Let me check.");
        assert_eq!(events[4]["text"], "Now answer.");
    }

    #[test]
    fn reasoning_summaries_show_as_thinking_unless_the_reasoning_was_streamed() {
        let log = EventLog::default();
        let summary = |text: &str| AgentEvent::ReasoningSummary {
            round: 1,
            text: text.into(),
        };
        log.text_delta(ResponseDelta::Reasoning("Planning."));
        log.record(&summary("Planning."));
        log.record(&summary("Only a summary."));
        let (backlog, _) = log.subscribe(0);
        let events: Vec<Value> = backlog
            .iter()
            .map(|event| serde_json::from_str(&event.data).unwrap())
            .collect();
        assert_eq!(
            events,
            [
                json!({"type": "thinking", "text": "Planning."}),
                json!({"type": "thinking", "text": "Only a summary."}),
            ]
        );
    }
}
