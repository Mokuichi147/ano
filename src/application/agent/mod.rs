//! The agent run loop: send a request, execute the returned tool calls, and
//! feed their results back until the model gives a final answer.

mod discovery;
mod dispatch;
mod events;
mod mcp_runtime;
mod response;
#[cfg(test)]
mod tests;

pub use discovery::task_plan_definition;
pub use events::{AgentEvent, EventListener};

use crate::{
    application::{
        input::{build_user_input, InputPart},
        ports::{ApprovalHandler, ConversationStore, McpGateway, ResponsesApi},
        registry::ToolRegistry,
        settings::AgentSettings,
    },
    domain::{
        compaction::{compacted_history, compaction_due},
        plan::{RunOutcome, TaskPlan, TASK_PLAN_NAME},
        policy::UserPolicy,
        session::{SessionBinding, SessionStatus},
        tool::ToolContext,
        usage::{ApiOperation, StopReason, UsageSummary},
    },
};
use anyhow::{bail, Context, Result};
use discovery::ActiveTools;
use dispatch::RoundScope;
use events::EventLog;
use mcp_runtime::McpRuntime;
use response::{extract_output_text, validate_response_status};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub input: Vec<InputPart>,
    pub context: ToolContext,
}

impl RunRequest {
    pub fn new(input: Vec<InputPart>) -> Self {
        Self {
            input,
            context: ToolContext::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentResult {
    pub text: String,
    pub response_id: String,
    pub events: Vec<AgentEvent>,
    pub outcome: RunOutcome,
    pub plan: TaskPlan,
    pub usage: UsageSummary,
    pub stop_reason: StopReason,
}

pub struct Agent {
    client: Arc<dyn ResponsesApi>,
    settings: AgentSettings,
    mcp: Arc<dyn McpGateway>,
    registry: ToolRegistry,
    policy: UserPolicy,
    approval_handler: Arc<dyn ApprovalHandler>,
    /// Serializes approval requests from concurrently running tool calls so an
    /// interactive handler never sees overlapping prompts.
    approval_lock: tokio::sync::Mutex<()>,
    event_listener: Option<EventListener>,
}

impl Agent {
    /// Create an agent. Share one MCP gateway (such as an `McpPool`) between
    /// agents, for example one per webhook job, so MCP connections are reused
    /// across runs.
    pub fn new(
        client: impl ResponsesApi + 'static,
        settings: AgentSettings,
        mcp: Arc<dyn McpGateway>,
        registry: ToolRegistry,
        policy: UserPolicy,
        approval_handler: Arc<dyn ApprovalHandler>,
    ) -> Self {
        Self {
            client: Arc::new(client),
            settings,
            mcp,
            registry,
            policy,
            approval_handler,
            approval_lock: tokio::sync::Mutex::new(()),
            event_listener: None,
        }
    }

    pub fn with_event_listener(mut self, listener: EventListener) -> Self {
        self.event_listener = Some(listener);
        self
    }

    pub async fn run(&self, request: RunRequest) -> Result<AgentResult> {
        self.run_inner(request, None).await
    }

    /// Continue a locally persisted conversation. Current tool policy is
    /// applied again; past tool calls are history, never scheduled for replay.
    pub async fn run_in_session(
        &self,
        request: RunRequest,
        session: &mut dyn ConversationStore,
    ) -> Result<AgentResult> {
        if session.data().binding != SessionBinding::new(&request.context, self.client.base_url())?
        {
            bail!("run context does not match this session");
        }
        let result = self.run_inner(request, Some(&mut *session)).await;
        if let Err(error) = &result {
            if session.data().status == SessionStatus::Running {
                session
                    .fail(&format!("{error:#}"))
                    .context("run failed and session checkpoint could not be saved")?;
            }
        }
        result
    }

    async fn run_inner(
        &self,
        request: RunRequest,
        mut session: Option<&mut (dyn ConversationStore + '_)>,
    ) -> Result<AgentResult> {
        self.settings.validate()?;

        let user_input = build_user_input(&request.input).await?;
        if let Some(session) = session.as_deref_mut() {
            session.begin_turn(&user_input)?;
        }
        let mcp_runtime =
            McpRuntime::new(self.mcp.connect(&self.policy).await?, self.policy.clone());
        let mut local_history = (session.is_none()
            && self.settings.compact_threshold_bytes.is_some())
        .then(|| user_input.as_array().cloned().unwrap_or_default());
        let mut previous_compact_size = session.as_ref().and_then(|session| {
            session
                .data()
                .compactions
                .last()
                .map(|record| record.after_bytes)
        });
        let mut usage = UsageSummary::default();
        let mut next_input = user_input;
        let mut previous_response_id: Option<String> = None;
        let events = EventLog::new(self.event_listener.as_ref());
        let plan = Mutex::new(
            session
                .as_ref()
                .map(|session| session.data().plan.clone())
                .unwrap_or_default(),
        );
        let mut active = ActiveTools::default();

        for round in 0..self.settings.max_tool_rounds {
            let history = session
                .as_ref()
                .map(|session| &session.data().history)
                .or(local_history.as_ref());
            if let Some(history) = history {
                if compaction_due(
                    history,
                    self.settings.compact_threshold_bytes,
                    previous_compact_size,
                )? {
                    let compact_payload = json!({"model":self.settings.model,"instructions":self.settings.instructions,"input":history});
                    let compacted = self.client.compact_response(&compact_payload).await
                        .context("context compaction failed; original history was preserved. Disable compact_threshold_bytes for endpoints without /responses/compact support")?;
                    observe_usage(
                        &compacted,
                        ApiOperation::Compaction,
                        round,
                        &mut usage,
                        session.as_deref_mut(),
                        &events,
                    )?;
                    let (history, mut record) = compacted_history(
                        &compacted,
                        compact_payload["input"].as_array().unwrap(),
                    )?;
                    previous_compact_size = Some(record.after_bytes);
                    if let Some(session) = session.as_deref_mut() {
                        record = session.replace_history(history, record)?;
                    } else {
                        local_history = Some(history);
                    }
                    events.push(AgentEvent::ContextCompacted { round, record });
                    if let Some(reason) = usage.stop_reason(self.settings.max_total_tokens) {
                        return finish_limited(
                            reason,
                            usage,
                            previous_response_id.unwrap_or_default(),
                            &plan,
                            events,
                            session,
                        );
                    }
                }
            }
            // Keep the request budget bounded while reserving one response to
            // explain completed work and any outstanding steps to the user.
            let final_round = round + 1 == self.settings.max_tool_rounds;
            let instructions = if final_round {
                format!(
                    "{}\n\nThis is the final response within the execution budget. No tools are available. Summarize what has actually been completed, clearly state anything unfinished, and do not claim unperformed work succeeded.",
                    self.settings.instructions
                )
            } else {
                self.settings.instructions.clone()
            };
            let tools = if final_round {
                Vec::new()
            } else {
                self.response_tools(&active, &mcp_runtime)?
            };
            let mut payload = json!({
                "model": self.settings.model,
                "instructions": instructions,
                "input": match &session { Some(session) => Value::Array(session.data().history.clone()), None => local_history.as_ref().map(|history| Value::Array(history.clone())).unwrap_or_else(|| next_input.clone()) },
                "tools": tools,
                "tool_choice": if final_round { "none" } else { "auto" },
                "parallel_tool_calls": self.settings.parallel_tool_calls,
            });
            if let Some(max_output_tokens) = self.settings.max_output_tokens {
                payload["max_output_tokens"] = json!(max_output_tokens);
            }
            if session.is_some() || local_history.is_some() {
                payload["store"] = json!(false);
                payload["include"] = json!(["reasoning.encrypted_content"]);
            } else if let Some(previous_response_id) = &previous_response_id {
                payload["previous_response_id"] = json!(previous_response_id);
            }

            let response = self.client.create_response(&payload).await?;
            observe_usage(
                &response,
                ApiOperation::Response,
                round,
                &mut usage,
                session.as_deref_mut(),
                &events,
            )?;
            if let Some(error) = response["error"]["message"].as_str() {
                bail!("Responses API returned an error: {error}");
            }
            // A truncated or unfinished response can contain complete-looking
            // text and tool calls. Validate it before executing any local work.
            validate_response_status(&response)?;
            let response_id = response["id"]
                .as_str()
                .context("Responses API response did not contain an id")?
                .to_string();
            previous_response_id = Some(response_id.clone());

            let mut items = response["output"].as_array().cloned().unwrap_or_default();
            if !items.iter().any(|item| item["type"] == "message") {
                if let Some(text) = response["output_text"]
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
                {
                    items.push(json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":text}]}));
                }
            }
            if final_round
                && items.iter().any(|item| {
                    matches!(
                        item["type"].as_str(),
                        Some("function_call" | "mcp_approval_request")
                    )
                })
            {
                bail!("Responses API requested a tool during the final response; no additional tool calls were executed");
            }
            let has_mcp_call = items
                .iter()
                .any(|item| item["type"].as_str() == Some("mcp_call"));
            let messages = items
                .iter()
                .filter(|item| item["type"] == "message")
                .collect::<Vec<_>>();
            let only_commentary =
                !messages.is_empty() && messages.iter().all(|item| item["phase"] == "commentary");
            for message in messages.iter().filter(|item| item["phase"] == "commentary") {
                let text = extract_output_text(&json!({"output":[message]}));
                if !text.trim().is_empty() {
                    events.push(AgentEvent::AssistantProgress { round, text });
                }
            }
            if let Some(session) = session.as_deref_mut() {
                session.record_response(&response_id, &items)?;
            }
            if let Some(history) = local_history.as_mut() {
                history.extend_from_slice(&items);
            }
            if let Some(reason) = usage.stop_reason(self.settings.max_total_tokens) {
                return finish_limited(reason, usage, response_id, &plan, events, session);
            }
            let (continuation, selection) = self
                .handle_output_items(
                    &items,
                    RoundScope {
                        round,
                        tool_context: &request.context,
                        active: &active,
                        mcp_runtime: &mcp_runtime,
                        events: &events,
                        plan: &plan,
                    },
                    session.as_deref_mut(),
                )
                .await?;
            if let Some(history) = local_history.as_mut() {
                history.extend_from_slice(&continuation);
            }
            if let Some(selection) = selection {
                active = selection;
            }

            // Commentary is progress, including when it shares a response with
            // a final message. Keep it out of the user's final answer.
            let final_messages = items
                .iter()
                .filter(|item| item["type"] == "message" && item["phase"] != "commentary")
                .cloned()
                .collect::<Vec<_>>();
            let output_text = extract_output_text(&json!({"output": final_messages}));
            if !continuation.is_empty() {
                if round + 1 >= self.settings.max_tool_rounds {
                    bail!("agent reached max_tool_rounds while tools were still pending");
                }
                next_input = Value::Array(continuation);
                continue;
            }

            if only_commentary {
                if final_round {
                    bail!("agent exhausted its request budget with a progress update instead of a final answer");
                }
                next_input = json!([]);
                continue;
            }

            if !output_text.trim().is_empty() {
                let plan = plan
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                let outcome = plan.outcome();
                if outcome == RunOutcome::Incomplete
                    && !final_round
                    && !self.policy.is_disabled(TASK_PLAN_NAME)
                {
                    next_input = json!([{"role":"user","content":[{"type":"input_text","text":"Runtime notice: your recorded task plan still has pending or in_progress steps. Continue the requested work and update task_plan before giving the final answer. If a step cannot proceed, mark it blocked with a concrete reason. Do not mark unperformed work completed just to end the run."}]}]);
                    if let Some(session) = session.as_deref_mut() {
                        session.record_runtime_input(&next_input)?;
                    }
                    if let Some(history) = local_history.as_mut() {
                        history.extend(next_input.as_array().unwrap().iter().cloned());
                    }
                    events.push(AgentEvent::AssistantProgress {
                        round,
                        text: "Continuing unfinished plan steps.".into(),
                    });
                    continue;
                }
                if let Some(session) = session.as_deref_mut() {
                    session.complete()?;
                }
                return Ok(AgentResult {
                    text: output_text,
                    response_id,
                    events: events.into_events(),
                    outcome,
                    plan,
                    usage,
                    stop_reason: if final_round {
                        StopReason::RoundLimit
                    } else {
                        StopReason::FinalAnswer
                    },
                });
            }

            // Remote MCP calls are normally followed by a message in the same
            // response. If a compatible endpoint returns only the call item,
            // give the model one chance to continue from the stored response.
            if has_mcp_call {
                if round + 1 >= self.settings.max_tool_rounds {
                    bail!("agent reached max_tool_rounds after an MCP call");
                }
                next_input = Value::Array(Vec::new());
                continue;
            }

            bail!("Responses API returned no assistant text");
        }

        bail!("agent reached max_tool_rounds")
    }
}

fn observe_usage(
    response: &Value,
    operation: ApiOperation,
    round: usize,
    usage: &mut UsageSummary,
    session: Option<&mut (dyn ConversationStore + '_)>,
    events: &EventLog<'_>,
) -> Result<()> {
    let delta = UsageSummary::from_response(response, operation);
    usage.add(&delta);
    if let Some(session) = session {
        session.record_usage(&delta)?;
    }
    if delta.unreported_requests == 0 {
        events.push(AgentEvent::UsageUpdated {
            round,
            operation,
            usage: usage.clone(),
        });
    }
    Ok(())
}

fn finish_limited(
    reason: StopReason,
    usage: UsageSummary,
    response_id: String,
    plan: &Mutex<TaskPlan>,
    events: EventLog<'_>,
    session: Option<&mut (dyn ConversationStore + '_)>,
) -> Result<AgentResult> {
    let text = match reason {
        StopReason::UsageUnavailable => "Execution stopped because the provider did not return valid token usage, so the configured token budget cannot be enforced. No further local tool calls were started. Completed actions are not undone.",
        _ => "Execution stopped at the configured token budget. No further local tool calls were started. Completed actions are not undone; unfinished work can be continued in another run.",
    }.to_string();
    if let Some(session) = session {
        session.skip_pending(&text)?;
        session.record_runtime_input(
            &json!([{"role":"user","content":[{"type":"input_text","text":text}]}]),
        )?;
        session.complete()?;
    }
    events.push(AgentEvent::ExecutionStopped {
        reason,
        usage: usage.clone(),
    });
    Ok(AgentResult {
        text,
        response_id,
        events: events.into_events(),
        outcome: RunOutcome::Incomplete,
        plan: plan
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone(),
        usage,
        stop_reason: reason,
    })
}
