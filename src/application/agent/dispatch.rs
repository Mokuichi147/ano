//! Execution of the tool calls and approval requests in one response.

use super::{
    discovery::ActiveTools,
    events::{AgentEvent, EventLog},
    extension::{AgentExtension, ExtensionCall, RunInfo, SubagentSpec},
    mcp_runtime::McpRuntime,
    response::{compact_output, parse_arguments, parse_mcp_approval},
    Agent, RunOrigin, RunRequest, DELEGATE_ROLE, SUBAGENT_INSTRUCTIONS,
};
use crate::{
    application::{
        input::InputPart,
        ports::{
            ApprovalDecision, ApprovalSource, ConversationStore, DirectMcpServer, DirectMcpTool,
            McpApprovalRequest,
        },
    },
    domain::{
        mcp::McpTransport,
        plan::{PlanChange, RunOutcome, TaskPlan, TASK_PLAN_NAME},
        tool::{ToolContext, ToolDefinition, DELEGATE_TASK_NAME, TOOL_SEARCH_NAME},
        usage::UsageSummary,
    },
};
use anyhow::{Context, Result};
use futures::{
    stream::{self, StreamExt},
    FutureExt,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

/// State shared by every output item of one Responses API round.
///
/// `active` is the tool selection the model saw when it produced the response;
/// a `tool_search` in the same response takes effect from the next request.
#[derive(Clone, Copy)]
pub(super) struct RoundScope<'a> {
    pub conversation: Option<&'a str>,
    pub call_id: Option<&'a str>,
    pub round: usize,
    pub user_request: &'a str,
    pub tool_context: &'a ToolContext,
    pub active: &'a ActiveTools,
    pub mcp_runtime: &'a McpRuntime,
    pub events: &'a EventLog<'a>,
    pub plan: &'a Mutex<TaskPlan>,
    /// Nesting of the run: 0, or 1 inside a sub-agent.
    pub depth: usize,
    /// Every call that needs approval is denied with this reason, as in a
    /// read-only reviewer's run.
    pub deny_approvals: Option<&'a str>,
    /// Tokens left in the run's budget, if it has one.
    pub token_budget: Option<u64>,
    /// Collects the usage of sub-agents started in this round.
    pub delegated_usage: &'a Mutex<UsageSummary>,
}

/// Result of handling one output item.
#[derive(Default)]
struct ItemOutcome {
    /// Item to send back in the next request's `input`.
    continuation: Option<Value>,
    /// 履歴にはモデル向けの短縮を行う前の結果を保存する。
    raw: Option<Value>,
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
                    session.checkpoint_tool_result_with_raw(
                        result,
                        outcome.raw.as_ref().unwrap_or(result),
                        &plan,
                    )?;
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
                    .handle_function_call(
                        name,
                        &item["arguments"],
                        RoundScope {
                            call_id: Some(call_id),
                            ..scope
                        },
                    )
                    .await?;
                Ok(ItemOutcome {
                    raw: Some(
                        json!({"type":"function_call_output", "call_id":call_id, "output":serde_json::to_string(&output)?}),
                    ),
                    continuation: Some(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": compact_output(&output, self.settings.max_tool_output_bytes),
                    })),
                    selection,
                })
            }
            "mcp_approval_request" => {
                let mut approval_request = parse_mcp_approval(item)?;
                approval_request.user_request = scope.user_request.to_string();
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
                    raw: None,
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
            ..
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
            let goal_left_out = !arguments["goal"].is_null() && !plan.has_user_goal();
            return Ok(match plan.apply(&arguments) {
                Ok(change) => {
                    if change == PlanChange::Updated {
                        events.push(AgentEvent::PlanUpdated {
                            round,
                            plan: plan.clone(),
                        });
                    }
                    (plan_result(&plan, change, goal_left_out), None)
                }
                Err(error) => (
                    json!({"error":"invalid_plan","message":format!("{error:#}"),"plan":*plan}),
                    None,
                ),
            });
        }

        if name == DELEGATE_TASK_NAME {
            return Ok((self.delegate_task(&arguments, scope).await, None));
        }

        if let Some((extension, definition)) = self.extension_for(name) {
            let arguments = definition.declared_arguments(arguments);
            let output = self
                .call_extension(extension.as_ref(), &definition, &arguments, scope)
                .await?;
            return Ok((output, None));
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
            return Ok(
                match self.search_tools(&arguments, mcp_runtime, tool_context) {
                    Ok(selection) => {
                        events.push(AgentEvent::ToolSearch {
                            round,
                            query: selection.query,
                            results: selection.results.clone(),
                        });
                        // A model can send its web query here, which matches
                        // no tool, and then give up on the capability.
                        let message = if selection.results.is_empty() {
                            "No tool matched. tool_search finds tools by their names and descriptions, usually written in English; it does not search the web. Search again with short English words for the capability you need, such as \"web search\" or \"fetch page\"."
                        } else {
                            "The returned tools are available from the next step; tool_search and task_plan remain available unless disabled. Call tool_search again when another capability is needed."
                        };
                        (
                            json!({
                                "tools": selection.results,
                                "message": message
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
                },
            );
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
        let Some(definition) = self.registry.definition(name) else {
            return Ok((
                json!({
                    "error": "tool_not_registered",
                    "tool": name,
                    "message": "The requested local tool is not registered. Use tool_search to find available tools."
                }),
                None,
            ));
        };
        let arguments = definition.declared_arguments(arguments);
        if !definition.always_offered && !active.local.contains(name) {
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

        if !definition.can_run(tool_context) {
            events.push(AgentEvent::LocalToolBlocked {
                round,
                name: name.to_string(),
            });
            return Ok((
                json!({
                    "error": "tool_unavailable",
                    "tool": name,
                    "message": "This tool is not enabled in the current environment."
                }),
                None,
            ));
        }
        let run = RunInfo {
            depth: scope.depth,
            context: tool_context,
            registry: &self.registry,
        };
        for extension in &self.extensions {
            if let Some(refusal) = extension.check_call(name, &arguments, &run).await {
                events.push(AgentEvent::LocalToolResult {
                    round,
                    name: name.to_string(),
                    output: refusal.clone(),
                });
                return Ok((refusal, None));
            }
        }
        if definition.requires_approval {
            let decision = self
                .request_approval(
                    scope.deny_approvals,
                    McpApprovalRequest {
                        approval_request_id: uuid::Uuid::new_v4().to_string(),
                        source: ApprovalSource::LocalTool,
                        tool_name: name.to_string(),
                        arguments: arguments.clone(),
                        tool_description: Some(definition.description.clone()),
                        user_request: scope.user_request.to_string(),
                        ..McpApprovalRequest::default()
                    },
                )
                .await?;
            events.push(AgentEvent::LocalToolApproval {
                round,
                name: name.to_string(),
                approved: decision.approved,
                reason: decision.reason.clone(),
            });
            if !decision.approved {
                let output = json!({
                    "error": "approval_denied",
                    "tool": name,
                    "message": denial_message(decision.reason.as_deref(), "call"),
                });
                events.push(AgentEvent::LocalToolResult {
                    round,
                    name: name.to_string(),
                    output: output.clone(),
                });
                return Ok((output, None));
            }
        }

        // A tool with its own deadline keeps its partial output when that
        // fires, before the generic guard does.
        let timeout_secs = definition
            .deadline_secs(&arguments, tool_context)
            .unwrap_or(self.settings.tool_timeout_secs);
        let arguments = self
            .extensions
            .iter()
            .fold(arguments, |arguments, extension| {
                extension.prepare_call(name, arguments, &run)
            });
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

    /// Run `task` in a sub-agent with a fresh conversation and return its
    /// final report. Failures are reported to the model, not raised.
    ///
    /// The sub-agent runs the same loop, which makes this future recursive;
    /// the boxed `Send` future breaks the cycle in the auto-trait check.
    fn delegate_task<'a>(
        &'a self,
        arguments: &'a Value,
        scope: RoundScope<'a>,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + 'a>> {
        Box::pin(self.delegate_task_inner(arguments, scope))
    }

    async fn delegate_task_inner(&self, arguments: &Value, scope: RoundScope<'_>) -> Value {
        if let Some(refusal) = self.subagent_refusal(DELEGATE_TASK_NAME, scope) {
            return refusal;
        }
        let Some(task) = arguments["task"]
            .as_str()
            .map(str::trim)
            .filter(|task| !task.is_empty())
        else {
            return json!({
                "error": "invalid_arguments",
                "tool": DELEGATE_TASK_NAME,
                "message": "task must be a non-empty string."
            });
        };
        let spec = SubagentSpec {
            task: task.to_string(),
            context: scope.tool_context.clone(),
            instructions: SUBAGENT_INSTRUCTIONS.to_string(),
            role: DELEGATE_ROLE.to_string(),
            deny_approvals: None,
        };
        match self.run_subagent(spec, scope).await {
            Ok(result) => {
                let mut output = json!({
                    "report": result.text,
                    "outcome": result.outcome,
                    "stop_reason": result.stop_reason,
                });
                if !result.plan.steps.is_empty() {
                    output["plan"] = json!(result.plan.steps);
                }
                output
            }
            Err(error) => json!({
                "error": "subagent_failed",
                "tool": DELEGATE_TASK_NAME,
                "message": format!("{error:#}"),
            }),
        }
    }

    /// The extension that handles the runtime tool `name`, with its
    /// definition, if one does.
    pub(super) fn extension_for(
        &self,
        name: &str,
    ) -> Option<(&Arc<dyn AgentExtension>, ToolDefinition)> {
        self.extensions.iter().find_map(|extension| {
            let definition = extension
                .tools()
                .into_iter()
                .find(|definition| definition.name == name)?;
            Some((extension, definition))
        })
    }

    /// Run a call of an extension's tool as a runtime tool: like
    /// `delegate_task`, only `disabled_tools` of the policy applies, not an
    /// allowlist. The tool must be offered to this run and able to run in its
    /// context; approval and the deadline apply as to local tools. Only an
    /// approval handler failure is an error.
    async fn call_extension(
        &self,
        extension: &dyn AgentExtension,
        definition: &ToolDefinition,
        arguments: &Value,
        scope: RoundScope<'_>,
    ) -> Result<Value> {
        let name = definition.name.as_str();
        let call = ExtensionCall { agent: self, scope };
        let disabled = self.policy.is_disabled(name);
        if disabled
            || !extension.offers(name, &call.run())
            || !definition.can_run(scope.tool_context)
        {
            scope.events.push(AgentEvent::LocalToolBlocked {
                round: scope.round,
                name: name.into(),
            });
            return Ok(json!({
                "error": if disabled { "tool_disabled" } else { "tool_unavailable" },
                "tool": name,
                "message": if disabled {
                    "This tool is disabled for the current user."
                } else if scope.depth > 0 {
                    "This tool is not available to a sub-agent; do the work yourself."
                } else {
                    "This tool is not available in this run."
                }
            }));
        }
        if definition.requires_approval {
            let decision = self
                .request_approval(
                    scope.deny_approvals,
                    McpApprovalRequest {
                        approval_request_id: uuid::Uuid::new_v4().to_string(),
                        source: ApprovalSource::LocalTool,
                        tool_name: name.to_string(),
                        arguments: arguments.clone(),
                        tool_description: Some(definition.description.clone()),
                        user_request: scope.user_request.to_string(),
                        ..McpApprovalRequest::default()
                    },
                )
                .await?;
            scope.events.push(AgentEvent::LocalToolApproval {
                round: scope.round,
                name: name.to_string(),
                approved: decision.approved,
                reason: decision.reason.clone(),
            });
            if !decision.approved {
                return Ok(json!({
                    "error": "approval_denied",
                    "tool": name,
                    "message": denial_message(decision.reason.as_deref(), "call"),
                }));
            }
        }
        let output = extension.call_tool(name, arguments, call);
        // Runtime tools such as a review run a whole sub-agent, so only a
        // deadline the tool sets applies.
        Ok(
            match definition.deadline_secs(arguments, scope.tool_context) {
                Some(secs) => match self.with_tool_timeout(output.map(Ok), secs).await {
                    Ok(output) => output,
                    Err(error) => json!({
                        "error": "tool_execution_failed",
                        "tool": name,
                        "message": format!("{error:#}"),
                    }),
                },
                None => output.await,
            },
        )
    }

    /// Why the sub-agent tool `name` cannot run here, if it cannot.
    fn subagent_refusal(&self, name: &str, scope: RoundScope<'_>) -> Option<Value> {
        if !self.policy.is_disabled(name) && scope.depth == 0 {
            return None;
        }
        scope.events.push(AgentEvent::LocalToolBlocked {
            round: scope.round,
            name: name.into(),
        });
        Some(json!({
            "error": "tool_disabled",
            "tool": name,
            "message": if scope.depth > 0 {
                "A sub-agent cannot start another sub-agent; do the work yourself."
            } else {
                "This tool is disabled for the current user."
            }
        }))
    }

    /// Run the sub-agent `spec` describes with a fresh conversation,
    /// counting its usage into the round. A sub-agent cannot start another.
    pub(super) async fn run_subagent(
        &self,
        spec: SubagentSpec,
        scope: RoundScope<'_>,
    ) -> Result<super::AgentResult> {
        if scope.depth > 0 {
            anyhow::bail!("A sub-agent cannot start another sub-agent; do the work yourself.");
        }
        let RoundScope { round, events, .. } = scope;
        let spent = Mutex::new(UsageSummary::default());
        // A sub-agent works for the same user in the same workspace, with no
        // permission its parent lacks.
        let parent = scope.tool_context;
        let context = ToolContext {
            user_id: parent.user_id.clone(),
            environment: parent.environment.clone(),
            workspace: parent.workspace.clone(),
            allow_writes: spec.context.allow_writes && parent.allow_writes,
            allow_exec: spec.context.allow_exec && parent.allow_exec,
            checks: parent
                .checks
                .iter()
                .filter(|(name, _)| spec.context.checks.contains_key(*name))
                .map(|(name, check)| (name.clone(), check.clone()))
                .collect(),
        };
        let request = RunRequest {
            input: vec![InputPart::Text(spec.task.clone())],
            raw_input: None,
            context,
            goal: None,
        };
        let origin = RunOrigin {
            parent_conversation: scope.conversation.map(str::to_string),
            parent_call_id: scope.call_id.map(str::to_string),
            depth: scope.depth + 1,
            role: Some(spec.role),
            instructions: Some(spec.instructions),
            deny_approvals: spec
                .deny_approvals
                .or_else(|| scope.deny_approvals.map(str::to_string)),
            token_limit: scope.token_budget,
            user_request: Some(scope.user_request.to_string()),
            usage_sink: Some(&spent),
        };
        events.push(AgentEvent::SubagentStarted {
            round,
            task: spec.task,
            model: self.target(&origin).model.to_string(),
        });
        let result = self.run_inner(request, None, origin).await;
        let spent = spent
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        scope
            .delegated_usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .add(&spent);
        events.push(AgentEvent::SubagentFinished {
            round,
            outcome: result.as_ref().ok().map(|result| result.outcome),
            usage: spent,
            error: result.as_ref().err().map(|error| format!("{error:#}")),
        });
        result
    }

    /// Ask the approval handler, one request at a time.
    async fn request_approval(
        &self,
        deny: Option<&str>,
        request: McpApprovalRequest,
    ) -> Result<ApprovalDecision> {
        if let Some(reason) = deny {
            return Ok(ApprovalDecision {
                approved: false,
                reason: Some(reason.to_string()),
            });
        }
        let _guard = self.approval_lock.lock().await;
        self.approval_handler.decide(request).await
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
        let server = self.mcp.configs().iter().find(|server| {
            server.transport == McpTransport::Responses
                && server.label == request.server_label
                && !scope.mcp_runtime.is_unavailable(&server.label)
        });
        let permitted = server
            .is_some_and(|server| server.is_tool_allowed(&self.policy, &request.tool_name))
            && active
                .responses_mcp
                .get(&request.server_label)
                .is_some_and(|names| names.contains(&request.tool_name));

        let decision = if permitted {
            let mut request = request.clone();
            request.tool_description = server.and_then(|server| {
                server
                    .tool_catalog
                    .iter()
                    .flatten()
                    .find(|tool| tool.name == request.tool_name)
                    .and_then(|tool| tool.description.clone())
            });
            self.request_approval(scope.deny_approvals, request).await?
        } else {
            events.push(AgentEvent::McpToolBlocked {
                round,
                server_label: request.server_label.clone(),
                tool_name: request.tool_name.clone(),
            });
            ApprovalDecision {
                approved: false,
                reason: None,
            }
        };
        events.push(AgentEvent::McpApproval {
            round,
            server_label: request.server_label.clone(),
            tool_name: request.tool_name.clone(),
            approved: decision.approved,
            reason: decision.reason,
        });
        Ok(decision.approved)
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
                source: ApprovalSource::Mcp,
                server_label: server_label.clone(),
                tool_name: tool_name.clone(),
                arguments: arguments.clone(),
                tool_description: Some(tool.description.clone()),
                user_request: scope.user_request.to_string(),
            };
            let decision = self
                .request_approval(scope.deny_approvals, approval_request)
                .await?;
            events.push(AgentEvent::McpApproval {
                round,
                server_label: server_label.clone(),
                tool_name: tool_name.clone(),
                approved: decision.approved,
                reason: decision.reason.clone(),
            });
            if !decision.approved {
                let output = json!({
                    "error": "mcp_approval_denied",
                    "message": denial_message(decision.reason.as_deref(), "MCP tool call")
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

/// The `task_plan` result. It says in words that the call succeeded and what
/// to do next: models have taken an "incomplete" plan status for a failed
/// call and resent the same plan instead of doing the work. `goal_left_out`
/// says that the call sent a goal although the user set none.
fn plan_result(plan: &TaskPlan, change: PlanChange, goal_left_out: bool) -> Value {
    let recorded = match change {
        PlanChange::Read => format!("This is the current plan (revision {}).", plan.revision),
        PlanChange::Unchanged => format!(
            "Nothing changed: the plan already has these steps (revision {}). Sending the same plan again does not advance the work.",
            plan.revision
        ),
        PlanChange::Updated => format!(
            "The plan was saved as revision {}. This call succeeded.",
            plan.revision
        ),
    };
    let next = match plan.outcome() {
        RunOutcome::Incomplete => "Work remains: carry it out now with the other tools, and call task_plan again only when a step or criterion changes status. If no available tool can do it, mark it blocked with the reason and tell the user what is missing.",
        RunOutcome::Blocked => "The remaining work is blocked; tell the user what was done and what is blocked.",
        RunOutcome::Completed => "All recorded work is finished.",
    };
    let goal = if goal_left_out {
        " The goal you sent was left out: only the user sets a goal, so send goal=null and keep your own targets as steps."
    } else {
        ""
    };
    json!({
        "plan": plan,
        "changed": change == PlanChange::Updated,
        "message": format!("{recorded}{goal} {next}"),
    })
}

/// Tell the model that a call was denied and must not simply be retried.
fn denial_message(reason: Option<&str>, what: &str) -> String {
    let denial = match reason {
        Some(reason) => format!("This {what} was denied: {reason}"),
        None => format!("The user denied this {what}."),
    };
    format!(
        "{denial} Do not retry the same call; continue without it, or tell the user what you need."
    )
}
