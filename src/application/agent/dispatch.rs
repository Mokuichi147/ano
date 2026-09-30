//! Execution of the tool calls and approval requests in one response.

use super::{
    discovery::ActiveTools,
    events::{AgentEvent, EventLog},
    mcp_runtime::McpRuntime,
    response::{compact_output, parse_arguments, parse_mcp_approval},
    Agent, RunOrigin, RunRequest,
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
        tool::{
            ToolContext, DELEGATE_TASK_NAME, EXEC_DEFAULT_TIMEOUT_SECS, EXEC_MAX_TIMEOUT_SECS,
            GIT_COMMIT_PUSH_NAME, GIT_DIFF_NAME, REVIEW_CHANGES_NAME, TOOL_SEARCH_NAME,
            WORKSPACE_EXEC_NAME,
        },
        usage::UsageSummary,
    },
};
use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::Mutex,
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
    /// A reviewer's run: every call that needs approval is denied.
    pub read_only: bool,
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

        if name == REVIEW_CHANGES_NAME {
            return Ok((self.review_changes(&arguments, scope).await, None));
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
        let always_offered = self
            .registry
            .definition(name)
            .is_some_and(|definition| definition.always_offered);
        if !always_offered && !active.local.contains(name) {
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

        if !tool_context.can_run(name) {
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
        if name == GIT_COMMIT_PUSH_NAME {
            if let Some(refusal) = self.unreviewed_files(&arguments, tool_context).await {
                events.push(AgentEvent::LocalToolResult {
                    round,
                    name: name.to_string(),
                    output: refusal.clone(),
                });
                return Ok((refusal, None));
            }
        }
        if let Some(definition) = self
            .registry
            .definition(name)
            .filter(|definition| definition.requires_approval)
        {
            let decision = self
                .request_approval(
                    scope.read_only,
                    McpApprovalRequest {
                        approval_request_id: uuid::Uuid::new_v4().to_string(),
                        source: ApprovalSource::LocalTool,
                        tool_name: name.to_string(),
                        arguments: arguments.clone(),
                        tool_description: Some(definition.description),
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

        // Checks and commands have their own deadline and preserve partial
        // output on timeout. Let that deadline fire before the generic guard.
        let timeout_secs = match name {
            "workspace_check" => arguments["name"]
                .as_str()
                .and_then(|name| tool_context.checks.get(name))
                .map(|check| check.timeout_secs.saturating_add(5))
                .unwrap_or(self.settings.tool_timeout_secs),
            WORKSPACE_EXEC_NAME => arguments["timeout_secs"]
                .as_u64()
                .unwrap_or(EXEC_DEFAULT_TIMEOUT_SECS)
                .min(EXEC_MAX_TIMEOUT_SECS)
                .saturating_add(5),
            _ => self.settings.tool_timeout_secs,
        };
        // The commit checks the files again under the workspace lock, so an
        // edit racing this call cannot slip in: hand it what was reviewed.
        let arguments = if name == GIT_COMMIT_PUSH_NAME {
            self.with_reviewed_files(arguments, tool_context)
        } else {
            arguments
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
        match self
            .run_subagent(task, scope.tool_context.clone(), false, scope)
            .await
        {
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

    /// Run `task` in a sub-agent with a fresh conversation in `context`,
    /// counting its usage into the round. `reviewer` makes it the read-only
    /// reviewer of `review_changes`.
    async fn run_subagent(
        &self,
        task: &str,
        context: ToolContext,
        reviewer: bool,
        scope: RoundScope<'_>,
    ) -> Result<super::AgentResult> {
        let RoundScope { round, events, .. } = scope;
        let spent = Mutex::new(UsageSummary::default());
        let request = RunRequest {
            input: vec![InputPart::Text(task.to_string())],
            raw_input: None,
            context,
            goal: None,
        };
        let origin = RunOrigin {
            parent_conversation: scope.conversation.map(str::to_string),
            parent_call_id: scope.call_id.map(str::to_string),
            depth: scope.depth + 1,
            token_limit: scope.token_budget,
            user_request: Some(scope.user_request.to_string()),
            usage_sink: Some(&spent),
            reviewer,
        };
        events.push(AgentEvent::SubagentStarted {
            round,
            task: task.to_string(),
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

    /// Have a read-only reviewer in a fresh conversation review the
    /// uncommitted changes, and record the reviewed state of the files when
    /// they did not change during the review.
    fn review_changes<'a>(
        &'a self,
        arguments: &'a Value,
        scope: RoundScope<'a>,
    ) -> Pin<Box<dyn Future<Output = Value> + Send + 'a>> {
        Box::pin(self.review_changes_inner(arguments, scope))
    }

    async fn review_changes_inner(&self, arguments: &Value, scope: RoundScope<'_>) -> Value {
        let failure = |error: &str, message: String| json!({"error": error, "tool": REVIEW_CHANGES_NAME, "message": message});
        if let Some(refusal) = self.subagent_refusal(REVIEW_CHANGES_NAME, scope) {
            return refusal;
        }
        let Some(request) = arguments["request"]
            .as_str()
            .map(str::trim)
            .filter(|request| !request.is_empty())
        else {
            return failure(
                "invalid_arguments",
                "request must describe what the changes are meant to do.".into(),
            );
        };
        let context = scope.tool_context;
        let Some(workspace) = context.workspace.clone() else {
            return failure("review_unavailable", "no workspace is configured.".into());
        };
        let before = match self.workspace_changes(context).await {
            Ok(changes) if changes.files.is_empty() => {
                return failure(
                    "nothing_to_review",
                    "The workspace has no uncommitted changes.".into(),
                )
            }
            Ok(changes) => changes,
            Err(error) => return failure("review_unavailable", format!("{error:#}")),
        };
        let files: Vec<String> = before.files.keys().cloned().collect();
        let task = format!(
            "Review the uncommitted changes in this workspace.\n\n\
             The request the changes are meant to fulfil, as the implementing agent described it:\n\
             <request>\n{request}\n</request>\n\n\
             Changed files: {}\n\n\
             The diff{}:\n```diff\n{}\n```",
            serde_json::to_string(&files).unwrap_or_default(),
            if before.truncated {
                " (cut short; read the rest with git_diff and workspace_read)"
            } else {
                ""
            },
            before.diff
        );
        // The reviewer reads, runs the configured checks, and reports; it
        // cannot write, run commands, or get calls approved.
        let reviewer_context = ToolContext {
            allow_writes: false,
            allow_exec: false,
            ..context.clone()
        };
        let result = self
            .run_subagent(&task, reviewer_context, true, scope)
            .await;
        let report = match result {
            Ok(result) => result.text,
            Err(error) => return failure("review_failed", format!("{error:#}")),
        };
        let after = self.workspace_changes(context).await;
        let recorded = matches!(&after, Ok(after) if after.files == before.files);
        if recorded {
            self.reviews
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(workspace, before.files);
        }
        json!({
            "report": report,
            "reviewed_files": files,
            // The reviewer got a shortened diff and had to read the rest.
            "diff_truncated": before.truncated,
            "recorded": recorded,
            "next": if recorded {
                "Judge each finding on its merits: fix the ones that are right, and note why you reject the others (for example in the pull request description). git_commit_push commits only files as they were reviewed, so after any further change call review_changes again."
            } else {
                "The files changed while they were reviewed, so this review was not recorded; call review_changes again before git_commit_push."
            },
        })
    }

    /// The uncommitted changes of the workspace from `git_diff`.
    async fn workspace_changes(&self, context: &ToolContext) -> Result<WorkspaceChanges> {
        let output = self
            .registry
            .execute_with_context(GIT_DIFF_NAME, json!({}), context)
            .await?;
        Ok(WorkspaceChanges {
            files: file_hashes(&output),
            diff: output["diff"].as_str().unwrap_or_default().to_string(),
            truncated: output["diff_truncated"].as_bool().unwrap_or(false),
        })
    }

    /// `git_commit_push` arguments with `reviewed` set to the files of the
    /// last review of the workspace (or null), replacing any the model sent.
    fn with_reviewed_files(&self, mut arguments: Value, context: &ToolContext) -> Value {
        let reviewed = context.workspace.as_ref().and_then(|workspace| {
            self.reviews
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(workspace)
                .cloned()
        });
        if let Some(arguments) = arguments.as_object_mut() {
            arguments.insert("reviewed".into(), json!(reviewed));
        }
        arguments
    }

    /// Why `git_commit_push` must not commit its files yet: a file is not in
    /// the state the last `review_changes` saw, or there was no review.
    async fn unreviewed_files(&self, arguments: &Value, context: &ToolContext) -> Option<Value> {
        let refusal = |message: String| {
            Some(json!({
                "error": "review_required",
                "tool": GIT_COMMIT_PUSH_NAME,
                "message": message,
            }))
        };
        let Some(workspace) = &context.workspace else {
            return None; // The tool itself reports the missing workspace.
        };
        let Some(paths) = arguments.get("files").filter(|files| files.is_array()) else {
            return None; // The tool itself reports the invalid arguments.
        };
        let current = match self
            .registry
            .execute_with_context(GIT_DIFF_NAME, json!({ "paths": paths }), context)
            .await
        {
            Ok(output) => file_hashes(&output),
            Err(error) => {
                return refusal(format!(
                    "could not check that the files were reviewed: {error:#}"
                ))
            }
        };
        let reviewed = self
            .reviews
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(workspace)
            .cloned();
        let Some(reviewed) = reviewed else {
            return refusal(format!(
                "The changes have not been reviewed. Call {REVIEW_CHANGES_NAME} first, judge its findings, and then commit."
            ));
        };
        let unreviewed: Vec<&String> = current
            .iter()
            .filter(|(path, hash)| reviewed.get(*path) != Some(*hash))
            .map(|(path, _)| path)
            .collect();
        if unreviewed.is_empty() {
            return None;
        }
        refusal(format!(
            "These files changed after the last review or were not part of it: {}. Call {REVIEW_CHANGES_NAME} again before committing them.",
            serde_json::to_string(&unreviewed).unwrap_or_default()
        ))
    }

    /// Ask the approval handler, one request at a time.
    async fn request_approval(
        &self,
        read_only: bool,
        request: McpApprovalRequest,
    ) -> Result<ApprovalDecision> {
        if read_only {
            return Ok(ApprovalDecision {
                approved: false,
                reason: Some("a read-only reviewer cannot make calls that need approval".into()),
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
            server.transport == McpTransport::Responses && server.label == request.server_label
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
            self.request_approval(scope.read_only, request).await?
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
                review: None,
            };
            let decision = self
                .request_approval(scope.read_only, approval_request)
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

/// The uncommitted changes `review_changes` shows its reviewer.
struct WorkspaceChanges {
    /// Changed files with the sha256 of their content (null when deleted).
    files: BTreeMap<String, Value>,
    diff: String,
    truncated: bool,
}

/// `path -> sha256` from the `files` of a `git_diff` result.
fn file_hashes(output: &Value) -> BTreeMap<String, Value> {
    output["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| Some((file["path"].as_str()?.to_string(), file["sha256"].clone())))
        .collect()
}
