//! Execution of the tool calls and approval requests in one response.

use super::{
    discovery::ActiveTools,
    events::{AgentEvent, EventLog},
    mcp_runtime::McpRuntime,
    response::{compact_output, parse_arguments, parse_mcp_approval},
    Agent,
};
use crate::{
    application::ports::{ConversationStore, DirectMcpServer, DirectMcpTool, McpApprovalRequest},
    domain::{
        mcp::McpTransport,
        plan::{TaskPlan, TASK_PLAN_NAME},
        tool::{ToolContext, TOOL_SEARCH_NAME},
    },
};
use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use serde_json::{json, Value};
use std::{collections::BTreeSet, future::Future, sync::Mutex, time::Duration};

/// State shared by every output item of one Responses API round.
///
/// `active` is the tool selection the model saw when it produced the response;
/// a `tool_search` in the same response takes effect from the next request.
#[derive(Clone, Copy)]
pub(super) struct RoundScope<'a> {
    pub round: usize,
    pub tool_context: &'a ToolContext,
    pub active: &'a ActiveTools,
    pub mcp_runtime: &'a McpRuntime,
    pub events: &'a EventLog<'a>,
    pub plan: &'a Mutex<TaskPlan>,
}

/// Result of handling one output item.
#[derive(Default)]
struct ItemOutcome {
    /// Item to send back in the next request's `input`.
    continuation: Option<Value>,
    /// New tool selection produced by `tool_search`.
    selection: Option<ActiveTools>,
}

impl Agent {
    /// Handle the output items of one response, running up to
    /// `AgentSettings::tool_concurrency` of them at the same time.
    ///
    /// Continuation items keep the order of the response. When the response
    /// contains several `tool_search` calls, the last one wins.
    pub(super) async fn handle_output_items(
        &self,
        items: &[Value],
        scope: RoundScope<'_>,
        mut session: Option<&mut (dyn ConversationStore + '_)>,
    ) -> Result<(Vec<Value>, Option<ActiveTools>)> {
        // Collect the futures first: a lazily mapped iterator makes the
        // spawned run future fail the higher-ranked `Send` check.
        let pending = items
            .iter()
            .enumerate()
            .map(|(index, item)| async move { (index, self.handle_output_item(item, scope).await) })
            .collect::<Vec<_>>();
        let mut pending = stream::iter(pending).buffer_unordered(self.settings.tool_concurrency());
        let mut outcomes = Vec::new();
        while let Some((index, outcome)) = pending.next().await {
            if let (Some(session), Ok(outcome)) = (session.as_deref_mut(), &outcome) {
                if let Some(result) = &outcome.continuation {
                    let plan = scope
                        .plan
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    session.checkpoint_tool_result(result, &plan)?;
                }
            }
            outcomes.push((index, outcome));
        }
        outcomes.sort_by_key(|(index, _)| *index);

        let mut continuation = Vec::new();
        let mut selection = None;
        for (_, outcome) in outcomes {
            let outcome = outcome?;
            continuation.extend(outcome.continuation);
            if outcome.selection.is_some() {
                selection = outcome.selection;
            }
        }
        Ok((continuation, selection))
    }

    async fn handle_output_item(&self, item: &Value, scope: RoundScope<'_>) -> Result<ItemOutcome> {
        match item["type"].as_str().unwrap_or_default() {
            "function_call" => {
                let call_id = item["call_id"]
                    .as_str()
                    .or_else(|| item["id"].as_str())
                    .context("function_call did not contain call_id")?;
                let name = item["name"]
                    .as_str()
                    .context("function_call did not contain name")?;
                let (output, selection) = self
                    .handle_function_call(name, &item["arguments"], scope)
                    .await?;
                Ok(ItemOutcome {
                    continuation: Some(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": compact_output(&output),
                    })),
                    selection,
                })
            }
            "mcp_approval_request" => {
                let approval_request = parse_mcp_approval(item)?;
                let approved = self
                    .approve_responses_mcp_call(&approval_request, scope)
                    .await?;
                Ok(ItemOutcome {
                    continuation: Some(json!({
                        "type": "mcp_approval_response",
                        "approve": approved,
                        "approval_request_id": approval_request.approval_request_id,
                    })),
                    selection: None,
                })
            }
            _ => Ok(ItemOutcome::default()),
        }
    }

    /// Execute one `function_call` and return its output for the model, plus
    /// the new tool selection when the call was `tool_search`.
    ///
    /// Errors the model can recover from (bad arguments, blocked or failing
    /// tools) are returned as JSON output; only an approval handler failure
    /// aborts the run.
    pub(super) async fn handle_function_call(
        &self,
        name: &str,
        raw_arguments: &Value,
        scope: RoundScope<'_>,
    ) -> Result<(Value, Option<ActiveTools>)> {
        let RoundScope {
            round,
            tool_context,
            active,
            mcp_runtime,
            events,
            plan,
        } = scope;
        let arguments = match parse_arguments(raw_arguments) {
            Ok(arguments) => arguments,
            Err(error) => {
                return Ok((
                    json!({
                        "error": "invalid_arguments",
                        "tool": name,
                        "message": format!("{error:#}. Retry with a JSON object that matches the tool schema."),
                    }),
                    None,
                ))
            }
        };

        if name == TASK_PLAN_NAME {
            if self.policy.is_disabled(TASK_PLAN_NAME) {
                events.push(AgentEvent::LocalToolBlocked {
                    round,
                    name: name.into(),
                });
                return Ok((json!({"error":"tool_disabled","tool":name}), None));
            }
            let mut plan = plan.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            return Ok(match plan.apply(&arguments) {
                Ok(changed) => {
                    if changed {
                        events.push(AgentEvent::PlanUpdated {
                            round,
                            plan: plan.clone(),
                        });
                    }
                    (json!({"plan":*plan,"outcome":plan.outcome()}), None)
                }
                Err(error) => (
                    json!({"error":"invalid_plan","message":format!("{error:#}"),"plan":*plan}),
                    None,
                ),
            });
        }

        if name == TOOL_SEARCH_NAME {
            if self.policy.is_disabled(TOOL_SEARCH_NAME) {
                events.push(AgentEvent::LocalToolBlocked {
                    round,
                    name: name.to_string(),
                });
                return Ok((
                    json!({
                        "error": "tool_disabled",
                        "tool": name,
                        "message": "Tool discovery is disabled for the current user."
                    }),
                    None,
                ));
            }
            return Ok(match self.search_tools(&arguments, mcp_runtime) {
                Ok(selection) => {
                    events.push(AgentEvent::ToolSearch {
                        round,
                        query: selection.query,
                        results: selection.results.clone(),
                    });
                    (
                        json!({
                            "tools": selection.results,
                            "message": "The returned tools are available from the next step; tool_search and task_plan remain available unless disabled. Call tool_search again when another capability is needed."
                        }),
                        Some(selection.active),
                    )
                }
                Err(error) => (
                    json!({
                        "error": "tool_search_failed",
                        "message": error.to_string()
                    }),
                    None,
                ),
            });
        }

        if let Some((server, tool)) = mcp_runtime.find_tool(name) {
            let output = self
                .execute_direct_mcp_tool(name, server, tool, &arguments, &active.direct_mcp, scope)
                .await?;
            return Ok((output, None));
        }

        events.push(AgentEvent::LocalToolCall {
            round,
            name: name.to_string(),
            arguments: arguments.clone(),
        });
        if self.policy.is_disabled(name) || !self.policy.is_allowed(name) {
            events.push(AgentEvent::LocalToolBlocked {
                round,
                name: name.to_string(),
            });
            return Ok((
                json!({
                    "error": "tool_disabled",
                    "tool": name,
                    "message": "This tool is disabled for the current user."
                }),
                None,
            ));
        }
        if !self.registry.is_registered(name) {
            return Ok((
                json!({
                    "error": "tool_not_registered",
                    "tool": name,
                    "message": "The requested local tool is not registered. Use tool_search to find available tools."
                }),
                None,
            ));
        }
        if !active.local.contains(name) {
            events.push(AgentEvent::LocalToolBlocked {
                round,
                name: name.to_string(),
            });
            return Ok((
                json!({
                    "error": "tool_not_selected",
                    "tool": name,
                    "message": "This tool was not selected by tool_search. Search for it and wait for the next step before calling it."
                }),
                None,
            ));
        }

        // Configured checks have their own deadline and preserve partial output
        // on timeout. Let that deadline fire before the generic tool guard.
        let timeout_secs = if name == "workspace_check" {
            arguments["name"]
                .as_str()
                .and_then(|name| tool_context.checks.get(name))
                .map(|check| check.timeout_secs.saturating_add(5))
                .unwrap_or(self.settings.tool_timeout_secs)
        } else {
            self.settings.tool_timeout_secs
        };
        let output = match self
            .with_tool_timeout(
                self.registry
                    .execute_with_context(name, arguments, tool_context),
                timeout_secs,
            )
            .await
        {
            Ok(value) => value,
            Err(error) => json!({
                "error": "tool_execution_failed",
                "tool": name,
                "message": format!("{error:#}")
            }),
        };
        events.push(AgentEvent::LocalToolResult {
            round,
            name: name.to_string(),
            output: output.clone(),
        });
        Ok((output, None))
    }

    /// Ask the approval handler, one request at a time.
    async fn request_approval(&self, request: McpApprovalRequest) -> Result<bool> {
        let _guard = self.approval_lock.lock().await;
        self.approval_handler.approve(request).await
    }

    async fn with_tool_timeout<F>(&self, future: F, timeout_secs: u64) -> Result<Value>
    where
        F: Future<Output = Result<Value>>,
    {
        let limit = Duration::from_secs(timeout_secs);
        tokio::time::timeout(limit, future)
            .await
            .map_err(|_| anyhow::anyhow!("tool call timed out after {}s", limit.as_secs()))?
    }

    /// Decide an approval request for a Responses-managed MCP call. A call the
    /// policy or the current tool selection does not permit is denied without
    /// asking the handler, even if the provider requested it.
    pub(super) async fn approve_responses_mcp_call(
        &self,
        request: &McpApprovalRequest,
        scope: RoundScope<'_>,
    ) -> Result<bool> {
        let RoundScope {
            round,
            active,
            events,
            ..
        } = scope;
        let permitted = self
            .mcp
            .configs()
            .iter()
            .find(|server| {
                server.transport == McpTransport::Responses && server.label == request.server_label
            })
            .is_some_and(|server| server.is_tool_allowed(&self.policy, &request.tool_name))
            && active
                .responses_mcp
                .get(&request.server_label)
                .is_some_and(|names| names.contains(&request.tool_name));

        let approved = if permitted {
            self.request_approval(request.clone()).await?
        } else {
            events.push(AgentEvent::McpToolBlocked {
                round,
                server_label: request.server_label.clone(),
                tool_name: request.tool_name.clone(),
            });
            false
        };
        events.push(AgentEvent::McpApproval {
            round,
            server_label: request.server_label.clone(),
            tool_name: request.tool_name.clone(),
            approved,
        });
        Ok(approved)
    }

    async fn execute_direct_mcp_tool(
        &self,
        function_name: &str,
        server: &dyn DirectMcpServer,
        tool: &DirectMcpTool,
        arguments: &Value,
        active_tools: &BTreeSet<String>,
        scope: RoundScope<'_>,
    ) -> Result<Value> {
        let RoundScope {
            round,
            mcp_runtime,
            events,
            ..
        } = scope;
        let server_label = server.config().label.clone();
        let tool_name = tool.name.clone();
        if !active_tools.contains(function_name)
            || !server.config().is_tool_allowed(&self.policy, &tool.name)
        {
            events.push(AgentEvent::McpToolBlocked {
                round,
                server_label,
                tool_name,
            });
            return Ok(json!({
                "error": "tool_disabled",
                "message": "This MCP tool is disabled or was not selected by tool_search."
            }));
        }

        events.push(AgentEvent::McpToolCall {
            round,
            server_label: server_label.clone(),
            tool_name: tool_name.clone(),
            arguments: arguments.clone(),
        });

        if server.config().requires_approval() {
            let approval_request = McpApprovalRequest {
                approval_request_id: uuid::Uuid::new_v4().to_string(),
                server_label: server_label.clone(),
                tool_name: tool_name.clone(),
                arguments: arguments.clone(),
            };
            let approved = self.request_approval(approval_request).await?;
            events.push(AgentEvent::McpApproval {
                round,
                server_label: server_label.clone(),
                tool_name: tool_name.clone(),
                approved,
            });
            if !approved {
                let output = json!({
                    "error": "mcp_approval_denied",
                    "message": "The user denied this MCP tool call."
                });
                events.push(AgentEvent::McpToolResult {
                    round,
                    server_label,
                    tool_name,
                    output: output.clone(),
                });
                return Ok(output);
            }
        }

        let output = match self
            .with_tool_timeout(
                mcp_runtime.call_tool(function_name, arguments),
                self.settings.tool_timeout_secs,
            )
            .await
        {
            Ok(value) => value,
            Err(error) => json!({
                "error": "mcp_tool_execution_failed",
                "message": format!("{error:#}")
            }),
        };
        events.push(AgentEvent::McpToolResult {
            round,
            server_label,
            tool_name,
            output: output.clone(),
        });
        Ok(output)
    }
}
