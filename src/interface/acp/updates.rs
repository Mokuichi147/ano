//! What a session shows in the editor: the run loop's events and the text the
//! model streams, as ACP session updates.

use crate::{
    application::{agent::AgentEvent, ports::ResponseDelta},
    domain::plan::{StepStatus, TaskPlan},
    infrastructure::tools::names::{
        GIT_COMMIT_PUSH_NAME, GIT_DIFF_NAME, REVIEW_CHANGES_NAME, WEB_FETCH_NAME,
        WORKSPACE_CHECK_NAME, WORKSPACE_EDIT_NAME, WORKSPACE_EXEC_NAME, WORKSPACE_READ_NAME,
        WORKSPACE_WRITE_NAME,
    },
    interface::shorten,
};
use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, Diff, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    SessionUpdate, TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallLocation,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

/// Longer tool outputs are cut in what the editor shows.
const MAX_OUTPUT_CHARS: usize = 20_000;
/// Longer arguments shown in a tool call's title are cut.
const MAX_TITLE_CHARS: usize = 80;

/// Turns a session's events into updates, remembering the calls it has
/// announced so their results and approvals refer to them.
#[derive(Default)]
pub(super) struct Updates {
    workspace: Option<PathBuf>,
    /// Every id announced to the editor in this session.
    announced: HashSet<String>,
    /// Calls waiting for their result: the model's id (or the tool's name,
    /// when the model gave none) to the ids announced for them, oldest first.
    open: HashMap<String, Vec<String>>,
    /// Reasoning was streamed since the last reasoning summary, which then
    /// repeats it.
    reasoning_streamed: bool,
}

impl Updates {
    pub(super) fn new(workspace: Option<PathBuf>) -> Self {
        Self {
            workspace,
            ..Self::default()
        }
    }

    /// Text the model streams.
    pub(super) fn delta(&mut self, delta: ResponseDelta<'_>) -> Option<SessionUpdate> {
        match delta {
            ResponseDelta::Text(text) => Some(SessionUpdate::AgentMessageChunk(chunk(text))),
            ResponseDelta::Reasoning(text) => {
                self.reasoning_streamed = true;
                Some(SessionUpdate::AgentThoughtChunk(chunk(text)))
            }
            ResponseDelta::MessageDone => None,
        }
    }

    /// A message of the model that was not streamed.
    pub(super) fn message(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(chunk(text))
    }

    /// The start of a turn.
    pub(super) fn start_turn(&mut self) {
        self.reasoning_streamed = false;
    }

    /// The end of a turn: calls still open, such as one cancelled while it
    /// waited for approval, end as failed.
    pub(super) fn finish_turn(&mut self) -> Vec<SessionUpdate> {
        let mut ids: Vec<String> = self.open.drain().flat_map(|(_, ids)| ids).collect();
        ids.sort();
        ids.into_iter()
            .map(|id| {
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    id,
                    ToolCallUpdateFields::new().status(ToolCallStatus::Failed),
                ))
            })
            .collect()
    }

    /// An event of the run loop.
    pub(super) fn event(&mut self, event: &AgentEvent) -> Vec<SessionUpdate> {
        match event {
            AgentEvent::AssistantProgress { streamed: true, .. } => Vec::new(),
            AgentEvent::AssistantProgress { text, .. } => vec![Self::message(&paragraph(text))],
            AgentEvent::ReasoningSummary { text, .. } => {
                if std::mem::take(&mut self.reasoning_streamed) {
                    Vec::new()
                } else {
                    vec![SessionUpdate::AgentThoughtChunk(chunk(&paragraph(text)))]
                }
            }
            AgentEvent::RuntimeNotice { text, .. } => {
                vec![SessionUpdate::AgentThoughtChunk(chunk(&paragraph(
                    &format!("[ano] {text}"),
                )))]
            }
            AgentEvent::McpServerUnavailable {
                server_label,
                error,
            } => vec![SessionUpdate::AgentThoughtChunk(chunk(&paragraph(
                &format!("[ano] MCP server '{server_label}' is unavailable: {error}"),
            )))],
            AgentEvent::ContextCompacted { .. } => {
                vec![SessionUpdate::AgentThoughtChunk(chunk(&paragraph(
                    "[ano] The conversation was compacted.",
                )))]
            }
            AgentEvent::PlanUpdated { plan, .. } => vec![SessionUpdate::Plan(plan_update(plan))],
            AgentEvent::LocalToolCall {
                call_id,
                name,
                arguments,
                ..
            } => vec![self.call(call_id.as_deref(), name, name, arguments)],
            AgentEvent::McpToolCall {
                call_id,
                server_label,
                tool_name,
                arguments,
                ..
            } => {
                let title = format!("{server_label}: {tool_name}");
                vec![self.call(call_id.as_deref(), tool_name, &title, arguments)]
            }
            AgentEvent::LocalToolResult {
                call_id,
                name,
                output,
                ..
            } => self.result(call_id.as_deref(), name, name, output),
            AgentEvent::McpToolResult {
                call_id,
                server_label,
                tool_name,
                output,
                ..
            } => {
                let title = format!("{server_label}: {tool_name}");
                self.result(call_id.as_deref(), tool_name, &title, output)
            }
            AgentEvent::LocalToolBlocked { call_id, name, .. } => {
                self.blocked(call_id.as_deref(), name, name)
            }
            AgentEvent::McpToolBlocked {
                call_id,
                server_label,
                tool_name,
                ..
            } => self.blocked(
                call_id.as_deref(),
                tool_name,
                &format!("{server_label}: {tool_name}"),
            ),
            AgentEvent::ToolSearch { query, results, .. } => {
                let id = self.unique_id(&format!("tool_search-{}", uuid::Uuid::new_v4()));
                let names: Vec<&str> = results
                    .iter()
                    .filter_map(|result| result["name"].as_str())
                    .collect();
                let found = if names.is_empty() {
                    "No tool matched.".to_string()
                } else {
                    names.join(", ")
                };
                vec![SessionUpdate::ToolCall(
                    ToolCall::new(id, format!("tool_search: {}", cut(query, MAX_TITLE_CHARS)))
                        .kind(ToolKind::Search)
                        .status(ToolCallStatus::Completed)
                        .content(vec![text_content(found)]),
                )]
            }
            AgentEvent::SubagentStarted { call_id, task, .. } => {
                let key = call_id.as_deref().unwrap_or("delegate_task");
                let id = self.open_call(key);
                let first_line = task.lines().next().unwrap_or_default();
                vec![SessionUpdate::ToolCall(
                    ToolCall::new(
                        id,
                        format!("Sub-agent: {}", cut(first_line, MAX_TITLE_CHARS)),
                    )
                    .kind(ToolKind::Think)
                    .status(ToolCallStatus::InProgress)
                    .content(vec![text_content(task.clone())]),
                )]
            }
            AgentEvent::SubagentFinished { call_id, error, .. } => {
                let key = call_id.as_deref().unwrap_or("delegate_task");
                let Some(id) = self.close_call(key) else {
                    return Vec::new();
                };
                let mut fields = ToolCallUpdateFields::new().status(match error {
                    Some(_) => ToolCallStatus::Failed,
                    None => ToolCallStatus::Completed,
                });
                if let Some(error) = error {
                    fields = fields.content(vec![text_content(error.clone())]);
                }
                vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    id, fields,
                ))]
            }
            AgentEvent::LocalToolApproval { .. }
            | AgentEvent::McpApproval { .. }
            | AgentEvent::UsageUpdated { .. }
            | AgentEvent::ExecutionStopped { .. } => Vec::new(),
        }
    }

    /// The id of the call waiting for approval, and the update that announces
    /// it when the editor has not seen it: a call the Responses API makes
    /// itself, or one of a tool that announces no calls.
    pub(super) fn approval_target(
        &mut self,
        call_id: Option<&str>,
        approval_id: &str,
        target: &str,
        arguments: &Value,
    ) -> (ToolCallId, Option<SessionUpdate>) {
        if let Some(id) = call_id
            .and_then(|call_id| self.open.get(call_id))
            .and_then(|ids| ids.last())
        {
            return (ToolCallId::new(id.as_str()), None);
        }
        let id = self.unique_id(call_id.unwrap_or(approval_id));
        let call = self
            .tool_call(&id, target, target, arguments)
            .status(ToolCallStatus::Pending);
        (ToolCallId::new(id), Some(SessionUpdate::ToolCall(call)))
    }

    fn call(
        &mut self,
        call_id: Option<&str>,
        name: &str,
        title: &str,
        arguments: &Value,
    ) -> SessionUpdate {
        let id = self.open_call(call_id.unwrap_or(name));
        SessionUpdate::ToolCall(
            self.tool_call(&id, name, title, arguments)
                .status(ToolCallStatus::InProgress),
        )
    }

    fn tool_call(&self, id: &str, name: &str, title: &str, arguments: &Value) -> ToolCall {
        let mut raw_input = arguments.clone();
        shorten(&mut raw_input, MAX_OUTPUT_CHARS);
        ToolCall::new(id.to_string(), title_for(title, arguments))
            .kind(kind_of(name))
            .locations(self.locations(arguments))
            .content(self.proposed_change(name, arguments))
            .raw_input(raw_input)
    }

    fn result(
        &mut self,
        call_id: Option<&str>,
        name: &str,
        title: &str,
        output: &Value,
    ) -> Vec<SessionUpdate> {
        let failed = output.get("error").is_some();
        let status = if failed {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        };
        let mut raw_output = output.clone();
        shorten(&mut raw_output, MAX_OUTPUT_CHARS);
        let mut content = Vec::new();
        // An edit keeps showing its change; the output only says it was made.
        if failed || !matches!(name, WORKSPACE_EDIT_NAME | WORKSPACE_WRITE_NAME) {
            content.push(text_content(output_text(output)));
        }
        let fields = ToolCallUpdateFields::new()
            .status(status)
            .raw_output(raw_output);
        let fields = if content.is_empty() {
            fields
        } else {
            fields.content(content)
        };
        match self.close_call(call_id.unwrap_or(name)) {
            Some(id) => vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                id, fields,
            ))],
            // A result without a call, such as a refusal before it ran.
            None => {
                let id = self.unique_id(call_id.unwrap_or(name));
                let mut call = ToolCall::new(id.clone(), title.to_string()).kind(kind_of(name));
                call.update(fields);
                vec![SessionUpdate::ToolCall(call)]
            }
        }
    }

    fn blocked(&mut self, call_id: Option<&str>, name: &str, title: &str) -> Vec<SessionUpdate> {
        let reason = text_content(format!("{title} is not available in this session."));
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .content(vec![reason]);
        match self.close_call(call_id.unwrap_or(name)) {
            Some(id) => vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                id, fields,
            ))],
            None => {
                let id = self.unique_id(call_id.unwrap_or(name));
                let mut call = ToolCall::new(id, title.to_string()).kind(kind_of(name));
                call.update(fields);
                vec![SessionUpdate::ToolCall(call)]
            }
        }
    }

    /// Announce a call under `key` and return its id.
    fn open_call(&mut self, key: &str) -> String {
        let id = self.unique_id(key);
        self.open
            .entry(key.to_string())
            .or_default()
            .push(id.clone());
        id
    }

    /// The id of the oldest call open under `key`, which ends.
    fn close_call(&mut self, key: &str) -> Option<String> {
        let ids = self.open.get_mut(key)?;
        let id = (!ids.is_empty()).then(|| ids.remove(0));
        if ids.is_empty() {
            self.open.remove(key);
        }
        id
    }

    /// `wanted`, or a variant of it when an earlier call of the session
    /// already took it: the model may reuse ids across turns.
    fn unique_id(&mut self, wanted: &str) -> String {
        let mut id = wanted.to_string();
        let mut next = 2;
        while self.announced.contains(&id) {
            id = format!("{wanted}-{next}");
            next += 1;
        }
        self.announced.insert(id.clone());
        id
    }

    fn locations(&self, arguments: &Value) -> Vec<ToolCallLocation> {
        ["path", "from", "to"]
            .into_iter()
            .filter_map(|key| arguments[key].as_str())
            .map(|path| ToolCallLocation::new(self.resolve(path)))
            .collect()
    }

    fn resolve(&self, path: &str) -> PathBuf {
        match &self.workspace {
            Some(workspace) => workspace.join(path),
            None => PathBuf::from(path),
        }
    }

    /// The change an edit makes, shown as a diff while it runs and after.
    fn proposed_change(&self, name: &str, arguments: &Value) -> Vec<ToolCallContent> {
        let Some(path) = arguments["path"].as_str() else {
            return Vec::new();
        };
        let path = self.resolve(path);
        match name {
            WORKSPACE_WRITE_NAME => arguments["content"]
                .as_str()
                .map(|content| vec![diff(&path, None, content)])
                .unwrap_or_default(),
            WORKSPACE_EDIT_NAME => arguments["edits"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|edit| {
                    Some(diff(
                        &path,
                        Some(edit["old_text"].as_str()?),
                        edit["new_text"].as_str()?,
                    ))
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

fn diff(path: &Path, old: Option<&str>, new: &str) -> ToolCallContent {
    ToolCallContent::Diff(Diff::new(path, new).old_text(old.map(str::to_string)))
}

fn chunk(text: &str) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
}

/// A whole message after streamed text, kept apart from it.
fn paragraph(text: &str) -> String {
    format!("{}\n\n", text.trim_end())
}

fn text_content(text: String) -> ToolCallContent {
    ToolCallContent::from(ContentBlock::Text(TextContent::new(text)))
}

/// What kind of work a tool does, for the editor's icon.
fn kind_of(name: &str) -> ToolKind {
    match name {
        WORKSPACE_READ_NAME | GIT_DIFF_NAME | "workspace_list" | "skill_read" => ToolKind::Read,
        "workspace_search" | "workspace_find" => ToolKind::Search,
        WORKSPACE_EDIT_NAME | WORKSPACE_WRITE_NAME => ToolKind::Edit,
        "workspace_move" => ToolKind::Move,
        "workspace_delete" => ToolKind::Delete,
        WORKSPACE_EXEC_NAME | WORKSPACE_CHECK_NAME | GIT_COMMIT_PUSH_NAME => ToolKind::Execute,
        WEB_FETCH_NAME => ToolKind::Fetch,
        REVIEW_CHANGES_NAME => ToolKind::Think,
        name if name.starts_with("history_") => ToolKind::Search,
        _ => ToolKind::Other,
    }
}

/// `title`, followed by the argument that best says what the call does.
fn title_for(title: &str, arguments: &Value) -> String {
    let subject = ["command", "path", "url", "query", "pattern", "from", "name"]
        .into_iter()
        .find_map(|key| arguments[key].as_str())
        .map(|subject| subject.lines().next().unwrap_or_default());
    match subject {
        Some(subject) if !subject.is_empty() => {
            format!("{title}: {}", cut(subject, MAX_TITLE_CHARS))
        }
        _ => title.to_string(),
    }
}

/// A tool's output for a person: the error's message, or the output as
/// pretty JSON in a code block, cut when long.
fn output_text(output: &Value) -> String {
    if let Some(message) = output["message"]
        .as_str()
        .filter(|_| output["error"].is_string())
    {
        return format!(
            "{}: {}",
            output["error"].as_str().unwrap_or_default(),
            message
        );
    }
    let text = match output {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    let text = cut(&text, MAX_OUTPUT_CHARS);
    let fence = "`".repeat(longest_backtick_run(&text).max(2) + 1);
    format!("{fence}\n{text}\n{fence}")
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0)
}

fn cut(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// The plan's steps as the editor's plan. A blocked step stays pending.
pub(super) fn plan_update(plan: &TaskPlan) -> Plan {
    Plan::new(
        plan.steps
            .iter()
            .map(|step| {
                let status = match step.status {
                    StepStatus::Pending | StepStatus::Blocked => PlanEntryStatus::Pending,
                    StepStatus::InProgress => PlanEntryStatus::InProgress,
                    StepStatus::Completed => PlanEntryStatus::Completed,
                };
                let content = match (&step.detail, step.status) {
                    (Some(detail), StepStatus::Blocked) => {
                        format!("{} (blocked: {detail})", step.description)
                    }
                    (_, StepStatus::Blocked) => format!("{} (blocked)", step.description),
                    _ => step.description.clone(),
                };
                PlanEntry::new(content, PlanEntryPriority::Medium, status)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn to_json(update: &SessionUpdate) -> Value {
        serde_json::to_value(update).unwrap()
    }

    #[test]
    fn calls_and_their_results_share_an_id_and_repeated_ids_stay_apart() {
        let mut updates = Updates::new(Some(PathBuf::from("/work")));
        let call = |id: &str| AgentEvent::LocalToolCall {
            round: 1,
            call_id: Some(id.into()),
            name: WORKSPACE_READ_NAME.into(),
            arguments: json!({"path": "src/main.rs"}),
        };
        let result = |id: &str, output: Value| AgentEvent::LocalToolResult {
            round: 1,
            call_id: Some(id.into()),
            name: WORKSPACE_READ_NAME.into(),
            output,
        };

        let started = to_json(&updates.event(&call("call_1"))[0]);
        assert_eq!(started["sessionUpdate"], "tool_call");
        assert_eq!(started["toolCallId"], "call_1");
        assert_eq!(started["title"], "workspace_read: src/main.rs");
        assert_eq!(started["kind"], "read");
        assert_eq!(started["status"], "in_progress");
        assert_eq!(started["locations"][0]["path"], "/work/src/main.rs");

        let done =
            to_json(&updates.event(&result("call_1", json!({"content": "fn main() {}"})))[0]);
        assert_eq!(done["sessionUpdate"], "tool_call_update");
        assert_eq!(done["toolCallId"], "call_1");
        assert_eq!(done["status"], "completed");
        assert!(done["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("fn main() {}"));

        // A later turn whose model reuses the id gets a new one.
        let again = to_json(&updates.event(&call("call_1"))[0]);
        assert_eq!(again["toolCallId"], "call_1-2");
        let failed = to_json(
            &updates.event(&result(
                "call_1",
                json!({"error": "tool_execution_failed", "message": "no such file"}),
            ))[0],
        );
        assert_eq!(failed["toolCallId"], "call_1-2");
        assert_eq!(failed["status"], "failed");
        assert_eq!(
            failed["content"][0]["content"]["text"],
            "tool_execution_failed: no such file"
        );
    }

    #[test]
    fn edits_show_their_change_as_a_diff() {
        let mut updates = Updates::new(Some(PathBuf::from("/work")));
        let started = to_json(
            &updates.event(&AgentEvent::LocalToolCall {
                round: 1,
                call_id: Some("edit".into()),
                name: WORKSPACE_EDIT_NAME.into(),
                arguments: json!({"path": "a.txt", "edits": [{"old_text": "old", "new_text": "new"}]}),
            })[0],
        );
        assert_eq!(started["kind"], "edit");
        assert_eq!(
            started["content"][0],
            json!({"type": "diff", "path": "/work/a.txt", "oldText": "old", "newText": "new"})
        );
        let done = to_json(
            &updates.event(&AgentEvent::LocalToolResult {
                round: 1,
                call_id: Some("edit".into()),
                name: WORKSPACE_EDIT_NAME.into(),
                output: json!({"path": "a.txt", "replacements": 1}),
            })[0],
        );
        assert_eq!(done["status"], "completed");
        assert!(done.get("content").is_none());
    }

    #[test]
    fn approvals_refer_to_the_announced_call_or_announce_one() {
        let mut updates = Updates::new(None);
        updates.event(&AgentEvent::LocalToolCall {
            round: 1,
            call_id: Some("exec".into()),
            name: WORKSPACE_EXEC_NAME.into(),
            arguments: json!({"command": "cargo test"}),
        });
        let (id, announce) = updates.approval_target(
            Some("exec"),
            "approval",
            WORKSPACE_EXEC_NAME,
            &json!({"command": "cargo test"}),
        );
        assert_eq!(id.to_string(), "exec");
        assert!(announce.is_none());
        // A call the turn left open ends as failed.
        let ended = to_json(&updates.finish_turn()[0]);
        assert_eq!(ended["toolCallId"], "exec");
        assert_eq!(ended["status"], "failed");
        assert!(updates.finish_turn().is_empty());

        let (id, announce) =
            updates.approval_target(None, "mcpr_1", "github:create_issue", &json!({}));
        assert_eq!(id.to_string(), "mcpr_1");
        let announce = to_json(&announce.unwrap());
        // Pending is the default status, which goes without saying.
        assert!(announce
            .get("status")
            .is_none_or(|status| status == "pending"));
        assert_eq!(announce["title"], "github:create_issue");
    }

    #[test]
    fn reasoning_summaries_are_thoughts_unless_the_reasoning_was_streamed() {
        let mut updates = Updates::new(None);
        let summary = AgentEvent::ReasoningSummary {
            round: 1,
            text: "Planning.".into(),
        };
        assert_eq!(updates.event(&summary).len(), 1);
        updates.delta(ResponseDelta::Reasoning("Planning."));
        assert!(updates.event(&summary).is_empty());
        assert_eq!(updates.event(&summary).len(), 1);
        assert!(updates
            .event(&AgentEvent::AssistantProgress {
                round: 1,
                text: "shown".into(),
                streamed: true,
            })
            .is_empty());
    }

    #[test]
    fn plans_keep_blocked_steps_pending_with_the_reason() {
        let plan: TaskPlan = serde_json::from_value(json!({
            "revision": 2,
            "explanation": null,
            "steps": [
                {"id": "1", "description": "Read", "status": "completed", "detail": null},
                {"id": "2", "description": "Fix", "status": "blocked", "detail": "needs access"},
                {"id": "3", "description": "Test", "status": "in_progress", "detail": null}
            ]
        }))
        .unwrap();
        let plan = serde_json::to_value(plan_update(&plan)).unwrap();
        let entries: Vec<(&str, &str)> = plan["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["content"].as_str().unwrap(),
                    entry["status"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            entries,
            [
                ("Read", "completed"),
                ("Fix (blocked: needs access)", "pending"),
                ("Test", "in_progress")
            ]
        );
    }

    #[test]
    fn outputs_are_fenced_beyond_their_own_backticks() {
        let text = output_text(&json!("```rust\nfn main() {}\n```"));
        assert!(text.starts_with("````\n") && text.ends_with("\n````"));
    }
}
