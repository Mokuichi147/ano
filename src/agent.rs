use crate::{
    client::OpenAiClient,
    config::{AgentSettings, McpTransport},
    input::{build_user_input, InputPart},
    mcp::{ConnectedMcpServer, DirectMcpTool, McpPool, McpRuntime},
    policy::UserPolicy,
    tools::{ToolContext, ToolDefinition, ToolRegistry, TOOL_SEARCH_NAME},
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    LocalToolCall {
        round: usize,
        name: String,
        arguments: Value,
    },
    LocalToolResult {
        round: usize,
        name: String,
        output: Value,
    },
    LocalToolBlocked {
        round: usize,
        name: String,
    },
    McpToolCall {
        round: usize,
        server_label: String,
        tool_name: String,
        arguments: Value,
    },
    McpToolResult {
        round: usize,
        server_label: String,
        tool_name: String,
        output: Value,
    },
    McpToolBlocked {
        round: usize,
        server_label: String,
        tool_name: String,
    },
    McpApproval {
        round: usize,
        server_label: String,
        tool_name: String,
        approved: bool,
    },
    ToolSearch {
        round: usize,
        query: String,
        results: Vec<Value>,
    },
}

/// Receives each event as soon as it happens, so progress is visible during a
/// long run and is not lost when the run fails.
pub type EventListener = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct AgentResult {
    pub text: String,
    pub response_id: String,
    pub events: Vec<AgentEvent>,
}

#[derive(Debug, Clone)]
pub struct McpApprovalRequest {
    pub approval_request_id: String,
    pub server_label: String,
    pub tool_name: String,
    pub arguments: Value,
}

#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn approve(&self, request: McpApprovalRequest) -> Result<bool>;
}

pub struct AlwaysApprove;

#[async_trait]
impl ApprovalHandler for AlwaysApprove {
    async fn approve(&self, _request: McpApprovalRequest) -> Result<bool> {
        Ok(true)
    }
}

pub struct DenyApproval;

#[async_trait]
impl ApprovalHandler for DenyApproval {
    async fn approve(&self, _request: McpApprovalRequest) -> Result<bool> {
        Ok(false)
    }
}

/// Prompts on stderr and reads the answer from stdin. Only use this when
/// stdin is an interactive terminal.
pub struct InteractiveApproval;

#[async_trait]
impl ApprovalHandler for InteractiveApproval {
    async fn approve(&self, request: McpApprovalRequest) -> Result<bool> {
        let prompt = format!(
            "\nMCP approval requested: {}:{}\nArguments: {}\nAllow this call? [y/N] ",
            request.server_label, request.tool_name, request.arguments
        );
        tokio::task::spawn_blocking(move || {
            use std::io::{self, Write};
            eprint!("{prompt}");
            io::stderr().flush().ok();
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            Ok::<bool, std::io::Error>(matches!(
                answer.trim().to_ascii_lowercase().as_str(),
                "y" | "yes"
            ))
        })
        .await
        .context("interactive MCP approval task failed")?
        .context("failed to read MCP approval response")
    }
}

pub struct Agent {
    client: OpenAiClient,
    settings: AgentSettings,
    mcp: Arc<McpPool>,
    registry: ToolRegistry,
    policy: UserPolicy,
    approval_handler: Arc<dyn ApprovalHandler>,
    /// Serializes approval requests from concurrently running tool calls so an
    /// interactive handler never sees overlapping prompts.
    approval_lock: tokio::sync::Mutex<()>,
    event_listener: Option<EventListener>,
}

/// Tools loaded by the most recent `tool_search`.
#[derive(Default)]
struct ActiveTools {
    local: BTreeSet<String>,
    /// Responses-managed MCP server label -> selected tool names.
    responses_mcp: BTreeMap<String, Vec<String>>,
    /// Function aliases of directly connected MCP tools.
    direct_mcp: BTreeSet<String>,
}

/// Collects events from concurrently running tool calls. The listener is
/// called under the same lock, so it sees events one at a time and in the
/// same order as `AgentResult::events`.
struct EventLog<'a> {
    events: Mutex<Vec<AgentEvent>>,
    listener: Option<&'a EventListener>,
}

impl<'a> EventLog<'a> {
    fn new(listener: Option<&'a EventListener>) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            listener,
        }
    }

    fn push(&self, event: AgentEvent) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(listener) = self.listener {
            listener(&event);
        }
        events.push(event);
    }

    fn into_events(self) -> Vec<AgentEvent> {
        self.events
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// State shared by every output item of one Responses API round.
///
/// `active` is the tool selection the model saw when it produced the response;
/// a `tool_search` in the same response takes effect from the next request.
#[derive(Clone, Copy)]
struct RoundScope<'a> {
    round: usize,
    tool_context: &'a ToolContext,
    active: &'a ActiveTools,
    mcp_runtime: &'a McpRuntime,
    events: &'a EventLog<'a>,
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
    /// Create an agent. Share one `McpPool` between agents (for example,
    /// one per webhook job) so MCP connections are reused across runs.
    pub fn new(
        client: OpenAiClient,
        settings: AgentSettings,
        mcp: Arc<McpPool>,
        registry: ToolRegistry,
        policy: UserPolicy,
        approval_handler: Arc<dyn ApprovalHandler>,
    ) -> Self {
        Self {
            client,
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
        self.settings.validate()?;

        let user_input = build_user_input(&request.input).await?;
        let mcp_runtime = self.mcp.runtime(&self.policy).await?;
        let mut next_input = user_input;
        let mut previous_response_id: Option<String> = None;
        let events = EventLog::new(self.event_listener.as_ref());
        let mut active = ActiveTools::default();

        for round in 0..self.settings.max_tool_rounds {
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
                "input": next_input,
                "tools": tools,
                "tool_choice": if final_round { "none" } else { "auto" },
                "parallel_tool_calls": self.settings.parallel_tool_calls,
            });
            if let Some(max_output_tokens) = self.settings.max_output_tokens {
                payload["max_output_tokens"] = json!(max_output_tokens);
            }
            if let Some(previous_response_id) = &previous_response_id {
                payload["previous_response_id"] = json!(previous_response_id);
            }

            let response = self.client.create_response(&payload).await?;
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

            let items = response["output"].as_array().cloned().unwrap_or_default();
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
            let (continuation, selection) = self
                .handle_output_items(
                    &items,
                    RoundScope {
                        round,
                        tool_context: &request.context,
                        active: &active,
                        mcp_runtime: &mcp_runtime,
                        events: &events,
                    },
                )
                .await?;
            if let Some(selection) = selection {
                active = selection;
            }

            let output_text = extract_output_text(&response);
            if !continuation.is_empty() {
                if round + 1 >= self.settings.max_tool_rounds {
                    bail!("agent reached max_tool_rounds while tools were still pending");
                }
                next_input = Value::Array(continuation);
                continue;
            }

            if !output_text.trim().is_empty() {
                return Ok(AgentResult {
                    text: output_text,
                    response_id,
                    events: events.into_events(),
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

    /// Handle the output items of one response, running up to
    /// `AgentSettings::tool_concurrency` of them at the same time.
    ///
    /// Continuation items keep the order of the response. When the response
    /// contains several `tool_search` calls, the last one wins.
    async fn handle_output_items(
        &self,
        items: &[Value],
        scope: RoundScope<'_>,
    ) -> Result<(Vec<Value>, Option<ActiveTools>)> {
        // Collect the futures first: a lazily mapped iterator makes the
        // spawned run future fail the higher-ranked `Send` check.
        let pending = items
            .iter()
            .map(|item| self.handle_output_item(item, scope))
            .collect::<Vec<_>>();
        let outcomes = stream::iter(pending)
            .buffered(self.settings.tool_concurrency())
            .collect::<Vec<_>>()
            .await;

        let mut continuation = Vec::new();
        let mut selection = None;
        for outcome in outcomes {
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
    async fn handle_function_call(
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
                            "message": "Only the returned tools are available from the next step. Call tool_search again when another capability is needed."
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

        let output = match self
            .with_tool_timeout(
                self.registry
                    .execute_with_context(name, arguments, tool_context),
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

    async fn with_tool_timeout<F>(&self, future: F) -> Result<Value>
    where
        F: Future<Output = Result<Value>>,
    {
        let limit = Duration::from_secs(self.settings.tool_timeout_secs);
        tokio::time::timeout(limit, future)
            .await
            .map_err(|_| anyhow::anyhow!("tool call timed out after {}s", limit.as_secs()))?
    }

    /// Decide an approval request for a Responses-managed MCP call. A call the
    /// policy or the current tool selection does not permit is denied without
    /// asking the handler, even if the provider requested it.
    async fn approve_responses_mcp_call(
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

    fn response_tools(&self, active: &ActiveTools, mcp_runtime: &McpRuntime) -> Result<Vec<Value>> {
        let mut tools = Vec::new();
        if !self.policy.is_disabled(TOOL_SEARCH_NAME) {
            tools.push(tool_search_definition().as_response_tool());
        }

        tools.extend(
            self.registry
                .definitions(&self.policy)
                .into_iter()
                .filter(|definition| active.local.contains(&definition.name))
                .map(|definition| definition.as_response_tool()),
        );

        for server in self.mcp.configs() {
            if server.transport != McpTransport::Responses {
                continue;
            }
            let Some(selected_tools) = active.responses_mcp.get(&server.label) else {
                continue;
            };
            if let Some(tool) = server.to_response_tool(&self.policy, selected_tools)? {
                tools.push(tool);
            }
        }

        for (server, tool) in mcp_runtime.tools() {
            if !active.direct_mcp.contains(&tool.function_name) {
                continue;
            }
            let description = format!(
                "MCP tool '{}' on server '{}'. {}",
                tool.name, server.config.label, tool.description
            );
            tools.push(json!({
                "type": "function",
                "name": tool.function_name,
                "description": description,
                "parameters": tool.input_schema,
                "strict": false,
            }));
        }
        Ok(tools)
    }

    fn search_tools(
        &self,
        arguments: &Value,
        mcp_runtime: &McpRuntime,
    ) -> Result<ToolSearchSelection> {
        let query = arguments["query"]
            .as_str()
            .context("tool_search.query must be a string")?
            .trim()
            .to_string();
        let limit = self.settings.tool_discovery_limit.clamp(1, 64);
        let terms = query
            .split_whitespace()
            .map(|term| term.to_lowercase())
            .collect::<Vec<_>>();

        let mut candidates = Vec::new();
        for definition in self.registry.definitions(&self.policy) {
            if let Some(score) = score_candidate(
                &terms,
                &[definition.name.clone(), definition.description.clone()],
            ) {
                candidates.push(SearchCandidate {
                    kind: "function",
                    server_label: None,
                    name: definition.name,
                    description: definition.description,
                    score,
                    function_name: None,
                });
            }
        }

        for server in self.mcp.configs() {
            if server.transport != McpTransport::Responses {
                continue;
            }
            for catalog in server.discoverable_tools(&self.policy) {
                let description = catalog.description.unwrap_or_else(|| {
                    format!("MCP tool '{}' on server '{}'.", catalog.name, server.label)
                });
                let fields = vec![
                    catalog.name.clone(),
                    description.clone(),
                    server.label.clone(),
                    server.description.clone().unwrap_or_default(),
                ];
                if let Some(score) = score_candidate(&terms, &fields) {
                    candidates.push(SearchCandidate {
                        kind: "mcp",
                        server_label: Some(server.label.clone()),
                        name: catalog.name,
                        description,
                        score,
                        function_name: None,
                    });
                }
            }
        }

        for (server, tool) in mcp_runtime.tools() {
            let fields = vec![
                tool.name.clone(),
                tool.description.clone(),
                server.config.label.clone(),
                server.config.description.clone().unwrap_or_default(),
            ];
            if let Some(score) = score_candidate(&terms, &fields) {
                candidates.push(SearchCandidate {
                    kind: "mcp",
                    server_label: Some(server.config.label.clone()),
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    score,
                    function_name: Some(tool.function_name.clone()),
                });
            }
        }

        candidates.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.kind.cmp(right.kind))
                .then_with(|| left.name.cmp(&right.name))
        });
        candidates.truncate(limit);

        let mut active = ActiveTools::default();
        let mut results = Vec::new();
        for candidate in candidates {
            let mut result = json!({
                "kind": candidate.kind,
                "name": candidate.name,
                "description": truncate_description(&candidate.description),
            });
            if let Some(server_label) = candidate.server_label {
                result["server_label"] = json!(server_label.clone());
                if let Some(function_name) = candidate.function_name {
                    result["function_name"] = json!(function_name.clone());
                    active.direct_mcp.insert(function_name);
                } else {
                    active
                        .responses_mcp
                        .entry(server_label)
                        .or_default()
                        .push(candidate.name.clone());
                }
            } else {
                active.local.insert(candidate.name.clone());
            }
            results.push(result);
        }

        Ok(ToolSearchSelection {
            query,
            results,
            active,
        })
    }

    async fn execute_direct_mcp_tool(
        &self,
        function_name: &str,
        server: &ConnectedMcpServer,
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
        let server_label = McpRuntime::server_label(server).to_string();
        let tool_name = tool.name.clone();
        if !active_tools.contains(function_name)
            || !McpRuntime::is_tool_allowed(server, &self.policy, tool)
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

        if McpRuntime::tool_requires_approval(server) {
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
            .with_tool_timeout(mcp_runtime.call_tool(function_name, arguments))
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

struct SearchCandidate {
    kind: &'static str,
    server_label: Option<String>,
    name: String,
    description: String,
    score: usize,
    function_name: Option<String>,
}

struct ToolSearchSelection {
    query: String,
    results: Vec<Value>,
    active: ActiveTools,
}

fn tool_search_definition() -> ToolDefinition {
    ToolDefinition::new(
        TOOL_SEARCH_NAME,
        "Search the registered local tools and MCP tools by capability. Use this before attempting a tool that is not currently available; only the returned tools are loaded for the next step.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Capability, action, or data to search for"}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    )
}

fn score_candidate(terms: &[String], fields: &[String]) -> Option<usize> {
    if terms.is_empty() {
        return Some(0);
    }
    let haystack = fields
        .iter()
        .map(|field| field.to_lowercase())
        .collect::<Vec<_>>();
    let score = terms
        .iter()
        .map(|term| {
            haystack
                .iter()
                .enumerate()
                .filter(|(_, field)| field.contains(term.as_str()))
                .map(|(index, _)| if index == 0 { 3 } else { 1 })
                .sum::<usize>()
        })
        .sum::<usize>();
    (score > 0).then_some(score)
}

fn truncate_description(description: &str) -> String {
    let mut result = description.chars().take(240).collect::<String>();
    if description.chars().count() > 240 {
        result.push('…');
    }
    result
}

fn parse_arguments(value: &Value) -> Result<Value> {
    let arguments = match value {
        Value::String(text) => {
            serde_json::from_str(text).context("tool arguments were not valid JSON")?
        }
        Value::Null => json!({}),
        value => value.clone(),
    };
    if !arguments.is_object() {
        bail!("tool arguments must be a JSON object");
    }
    Ok(arguments)
}

fn compact_output(value: &Value) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "{\"error\":\"failed to serialize tool output\"}".to_string())
}

fn validate_response_status(response: &Value) -> Result<()> {
    match response.get("status") {
        // Some compatible endpoints omit status in otherwise valid responses.
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(status)) if status == "completed" => Ok(()),
        Some(Value::String(status)) if status == "incomplete" => {
            let reason = response["incomplete_details"]["reason"]
                .as_str()
                .unwrap_or("unknown");
            bail!("Responses API returned an incomplete response ({reason}); no local tool calls from this response were executed");
        }
        Some(Value::String(status)) => {
            bail!(
                "Responses API returned response status '{status}'; expected a completed response"
            );
        }
        Some(_) => bail!("Responses API returned an invalid response status"),
    }
}

fn extract_output_text(response: &Value) -> String {
    if let Some(text) = response["output_text"].as_str() {
        if !text.trim().is_empty() {
            return text.to_string();
        }
    }

    response["output"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"].as_str() == Some("message"))
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter_map(|item| match item["type"].as_str() {
            Some("output_text") => item["text"].as_str(),
            Some("refusal") => item["refusal"].as_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn parse_mcp_approval(item: &Value) -> Result<McpApprovalRequest> {
    let approval_request_id = item["approval_request_id"]
        .as_str()
        .or_else(|| item["id"].as_str())
        .context("MCP approval request did not contain an id")?
        .to_string();
    let server_label = item["server_label"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let tool_name = item["name"]
        .as_str()
        .or_else(|| item["tool_name"].as_str())
        .unwrap_or("unknown")
        .to_string();
    let arguments =
        parse_arguments(&item["arguments"]).unwrap_or_else(|_| item["arguments"].clone());

    Ok(McpApprovalRequest {
        approval_request_id,
        server_label,
        tool_name,
        arguments,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        extract_output_text, ActiveTools, Agent, AlwaysApprove, ApprovalHandler, DenyApproval,
        EventLog, McpApprovalRequest, RoundScope, RunRequest,
    };
    use crate::config::{McpApprovalMode, McpServerConfig, McpToolCatalog, McpTransport};
    use crate::mcp::{McpPool, McpRuntime};
    use crate::{
        AgentSettings, InputPart, OpenAiClient, ToolContext, ToolDefinition, ToolRegistry,
        UserPolicy,
    };
    use anyhow::Result;
    use async_trait::async_trait;
    use axum::{extract::State, routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };

    async fn with_scope<T>(active: &ActiveTools, body: impl AsyncFnOnce(RoundScope<'_>) -> T) -> T {
        let context = ToolContext::default();
        let runtime = McpRuntime::default();
        let events = EventLog::new(None);
        body(RoundScope {
            round: 0,
            tool_context: &context,
            active,
            mcp_runtime: &runtime,
            events: &events,
        })
        .await
    }

    fn registry_with(tools: &[(&str, &str)]) -> ToolRegistry {
        let registry = ToolRegistry::new();
        for (name, description) in tools {
            registry
                .register(
                    ToolDefinition::new(
                        *name,
                        *description,
                        json!({"type": "object", "properties": {}}),
                    ),
                    |_arguments| async move { Ok(json!({"ok": true})) },
                )
                .unwrap();
        }
        registry
    }

    fn agent(registry: ToolRegistry, mcp_servers: Vec<McpServerConfig>) -> Agent {
        Agent::new(
            OpenAiClient::new("test", "http://127.0.0.1:1234/v1"),
            AgentSettings::default(),
            Arc::new(McpPool::new(mcp_servers)),
            registry,
            UserPolicy::default(),
            Arc::new(AlwaysApprove),
        )
    }

    fn selected_local_tools(names: &[&str]) -> ActiveTools {
        ActiveTools {
            local: names.iter().map(|name| (*name).to_string()).collect(),
            ..ActiveTools::default()
        }
    }

    #[derive(Clone)]
    struct MockResponsesState {
        responses: Arc<Mutex<VecDeque<Value>>>,
        requests: Arc<Mutex<Vec<Value>>>,
    }

    struct MockResponses {
        url: String,
        requests: Arc<Mutex<Vec<Value>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockResponses {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn mock_response(
        State(state): State<MockResponsesState>,
        Json(payload): Json<Value>,
    ) -> Json<Value> {
        state.requests.lock().unwrap().push(payload);
        Json(
            state
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| json!({"error": {"message": "unexpected extra request"}})),
        )
    }

    async fn mock_responses(responses: Vec<Value>) -> MockResponses {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = MockResponsesState {
            responses: Arc::new(Mutex::new(responses.into())),
            requests: Arc::clone(&requests),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/v1/responses", post(mock_response))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        MockResponses {
            url,
            requests,
            task,
        }
    }

    fn text_response(id: &str, text: &str) -> Value {
        json!({
            "id": id,
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": text}]
            }]
        })
    }

    fn search_response(id: &str, query: &str) -> Value {
        json!({
            "id": id,
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": format!("{id}_search"),
                "name": "tool_search",
                "arguments": json!({"query": query}).to_string()
            }]
        })
    }

    fn counting_registry() -> (ToolRegistry, Arc<AtomicUsize>) {
        let registry = ToolRegistry::new();
        let count = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&count);
        registry
            .register(
                ToolDefinition::new(
                    "record_action",
                    "Record an action",
                    json!({"type": "object", "properties": {}, "additionalProperties": false}),
                ),
                move |_arguments| {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(json!({"saved": true}))
                    }
                },
            )
            .unwrap();
        (registry, count)
    }

    fn request() -> RunRequest {
        RunRequest::new(vec![InputPart::Text("Do the requested work".into())])
    }

    #[test]
    fn extracts_text_from_raw_response_items() {
        let response = json!({
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "done"}]
            }]
        });
        assert_eq!(extract_output_text(&response), "done");
    }

    #[test]
    fn initial_tool_payload_is_constant_size() {
        let agent = Agent::new(
            OpenAiClient::new("test", "http://127.0.0.1:1234/v1"),
            AgentSettings::default(),
            Arc::new(McpPool::new(Vec::new())),
            registry_with(&[("echo", "Return a value")]),
            UserPolicy::default(),
            Arc::new(DenyApproval),
        );

        let tools = agent
            .response_tools(&ActiveTools::default(), &McpRuntime::default())
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "tool_search");
    }

    #[test]
    fn search_loads_only_matching_tools() {
        let agent = agent(
            registry_with(&[
                ("read_file", "Read a file"),
                ("send_email", "Send an email"),
            ]),
            Vec::new(),
        );

        let selection = agent
            .search_tools(&json!({"query": "read file"}), &McpRuntime::default())
            .unwrap();
        assert!(selection.active.local.contains("read_file"));
        assert!(!selection.active.local.contains("send_email"));
        assert_eq!(selection.results.len(), 1);
    }

    #[test]
    fn search_returns_up_to_the_discovery_limit() {
        let agent = agent(
            registry_with(&[
                ("read_file", "Read a file"),
                ("read_dir", "Read a directory"),
                ("read_url", "Read a URL"),
            ]),
            Vec::new(),
        );

        let selection = agent
            .search_tools(&json!({"query": "read"}), &McpRuntime::default())
            .unwrap();
        assert_eq!(selection.results.len(), 3);
    }

    #[tokio::test]
    async fn invalid_arguments_are_returned_to_the_model() {
        let agent = agent(registry_with(&[("echo_tool", "Echo")]), Vec::new());
        let (output, _) = with_scope(&ActiveTools::default(), async |scope| {
            agent
                .handle_function_call("echo_tool", &json!("{not json"), scope)
                .await
        })
        .await
        .unwrap();
        assert_eq!(output["error"], "invalid_arguments");
    }

    /// Two tools that each wait for the other: they only finish when run at
    /// the same time.
    fn barrier_registry() -> ToolRegistry {
        let registry = ToolRegistry::new();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        for name in ["first", "second"] {
            let barrier = Arc::clone(&barrier);
            registry
                .register(
                    ToolDefinition::new(name, name, json!({"type": "object", "properties": {}})),
                    move |_arguments| {
                        let barrier = Arc::clone(&barrier);
                        async move {
                            barrier.wait().await;
                            Ok(json!({"tool": name}))
                        }
                    },
                )
                .unwrap();
        }
        registry
    }

    fn calls(names: &[&str]) -> Vec<Value> {
        names
            .iter()
            .map(|name| {
                json!({"type": "function_call", "call_id": format!("call_{name}"), "name": name, "arguments": "{}"})
            })
            .collect()
    }

    #[tokio::test]
    async fn runs_tool_calls_from_one_response_concurrently() {
        let agent = agent(barrier_registry(), Vec::new());
        let items = calls(&["first", "second"]);
        let active = selected_local_tools(&["first", "second"]);

        let (continuation, _) = with_scope(&active, async |scope| {
            tokio::time::timeout(
                Duration::from_secs(5),
                agent.handle_output_items(&items, scope),
            )
            .await
        })
        .await
        .expect("tool calls did not run concurrently")
        .unwrap();

        let call_ids = continuation
            .iter()
            .map(|item| item["call_id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(call_ids, ["call_first", "call_second"]);
    }

    #[tokio::test]
    async fn runs_tool_calls_sequentially_when_parallel_calls_are_disabled() {
        let mut agent = agent(barrier_registry(), Vec::new());
        agent.settings.parallel_tool_calls = false;
        let items = calls(&["first", "second"]);
        let active = selected_local_tools(&["first", "second"]);

        let result = with_scope(&active, async |scope| {
            tokio::time::timeout(
                Duration::from_millis(300),
                agent.handle_output_items(&items, scope),
            )
            .await
        })
        .await;
        assert!(result.is_err(), "calls should not overlap");
    }

    #[tokio::test]
    async fn tool_search_selection_applies_after_the_round() {
        let agent = agent(
            registry_with(&[
                ("read_file", "Read a file"),
                ("send_email", "Send an email"),
            ]),
            Vec::new(),
        );
        let mut items = vec![json!({
            "type": "function_call", "call_id": "search", "name": "tool_search",
            "arguments": "{\"query\": \"read\"}"
        })];
        items.extend(calls(&["send_email"]));

        let (continuation, selection) = with_scope(&ActiveTools::default(), async |scope| {
            agent.handle_output_items(&items, scope).await
        })
        .await
        .unwrap();
        assert_eq!(continuation.len(), 2);
        let blocked: Value =
            serde_json::from_str(continuation[1]["output"].as_str().unwrap()).unwrap();
        assert_eq!(blocked["error"], "tool_not_selected");
        assert!(selection.unwrap().local.contains("read_file"));
    }

    #[tokio::test]
    async fn rejects_unselected_calls_and_executes_selected_calls_over_http() {
        let server = mock_responses(vec![
            json!({"id": "unselected", "status": "completed", "output": calls(&["record_action"])}),
            search_response("search", "record_action"),
            json!({"id": "selected", "status": "completed", "output": calls(&["record_action"])}),
            text_response("done", "Action recorded"),
        ])
        .await;
        let (registry, count) = counting_registry();
        let mut agent = agent(registry, Vec::new());
        agent.client = OpenAiClient::new("test", &server.url);
        agent.settings.max_tool_rounds = 4;

        let result = agent.run(request()).await.unwrap();

        assert_eq!(result.text, "Action recorded");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let requests = server.requests.lock().unwrap();
        let blocked: Value =
            serde_json::from_str(requests[1]["input"][0]["output"].as_str().unwrap()).unwrap();
        assert_eq!(blocked["error"], "tool_not_selected");
        assert_eq!(requests[1]["previous_response_id"], "unselected");
        assert!(requests[2]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "record_action"));
        assert_eq!(requests[3]["tools"], json!([]));
        assert_eq!(requests[3]["tool_choice"], "none");
        let saved: Value =
            serde_json::from_str(requests[3]["input"][0]["output"].as_str().unwrap()).unwrap();
        assert_eq!(saved["saved"], true);
    }

    #[tokio::test]
    async fn a_new_search_revokes_previous_local_selection() {
        let server = mock_responses(vec![
            search_response("first", "record_action"),
            search_response("second", "no_matching_capability"),
            json!({"id": "stale", "status": "completed", "output": calls(&["record_action"])}),
            text_response("done", "No action executed"),
        ])
        .await;
        let (registry, count) = counting_registry();
        let mut agent = agent(registry, Vec::new());
        agent.client = OpenAiClient::new("test", &server.url);
        agent.settings.max_tool_rounds = 4;

        agent.run(request()).await.unwrap();

        assert_eq!(count.load(Ordering::SeqCst), 0);
        let requests = server.requests.lock().unwrap();
        let blocked: Value =
            serde_json::from_str(requests[3]["input"][0]["output"].as_str().unwrap()).unwrap();
        assert_eq!(blocked["error"], "tool_not_selected");
    }

    #[tokio::test]
    async fn unfinished_responses_never_execute_tools_or_succeed_with_partial_text() {
        for status in ["incomplete", "failed", "cancelled", "in_progress", "queued"] {
            for includes_tool in [false, true] {
                let mut response = text_response("unfinished", "This is only a partial answer");
                response["status"] = json!(status);
                response["incomplete_details"] = json!({"reason": "max_output_tokens"});
                if includes_tool {
                    response["output"]
                        .as_array_mut()
                        .unwrap()
                        .extend(calls(&["record_action"]));
                }
                let server =
                    mock_responses(vec![search_response("search", "record_action"), response])
                        .await;
                let (registry, count) = counting_registry();
                let mut agent = agent(registry, Vec::new());
                agent.client = OpenAiClient::new("test", &server.url);

                let error = agent.run(request()).await.unwrap_err();

                assert!(error.to_string().contains(status), "{error}");
                assert_eq!(count.load(Ordering::SeqCst), 0);
                assert_eq!(server.requests.lock().unwrap().len(), 2);
            }
        }
    }

    #[tokio::test]
    async fn returns_refusal_text_to_the_caller() {
        let server = mock_responses(vec![json!({
            "id": "refusal", "status": "completed", "output": [{
                "type": "message",
                "content": [{"type": "refusal", "refusal": "I cannot help with that request."}]
            }]
        })])
        .await;
        let mut agent = agent(ToolRegistry::new(), Vec::new());
        agent.client = OpenAiClient::new("test", &server.url);

        let result = agent.run(request()).await.unwrap();

        assert_eq!(result.text, "I cannot help with that request.");
        assert_eq!(result.response_id, "refusal");
    }

    #[tokio::test]
    async fn one_round_budget_requests_a_final_answer_without_tools() {
        let server = mock_responses(vec![text_response("done", "Here is my answer")]).await;
        let mut agent = agent(ToolRegistry::new(), Vec::new());
        agent.client = OpenAiClient::new("test", &server.url);
        agent.settings.max_tool_rounds = 1;

        assert_eq!(
            agent.run(request()).await.unwrap().text,
            "Here is my answer"
        );

        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["tools"], json!([]));
        assert_eq!(requests[0]["tool_choice"], "none");
    }

    #[tokio::test]
    async fn never_executes_calls_requested_during_the_final_response() {
        let server = mock_responses(vec![
            search_response("search", "record_action"),
            json!({"id": "final", "status": "completed", "output": calls(&["record_action"])}),
        ])
        .await;
        let (registry, count) = counting_registry();
        let mut agent = agent(registry, Vec::new());
        agent.client = OpenAiClient::new("test", &server.url);
        agent.settings.max_tool_rounds = 2;

        let error = agent.run(request()).await.unwrap_err();

        assert!(error.to_string().contains("final response"));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1]["tools"], json!([]));
        assert_eq!(requests[1]["tool_choice"], "none");
    }

    struct CountingApproval {
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    #[async_trait]
    impl ApprovalHandler for CountingApproval {
        async fn approve(&self, _request: McpApprovalRequest) -> Result<bool> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(true)
        }
    }

    #[tokio::test]
    async fn responses_mcp_approval_requires_policy_and_selection() {
        let server = McpServerConfig {
            label: "github".into(),
            transport: McpTransport::Responses,
            url: Some("https://example.test/mcp".into()),
            tunnel_id: None,
            command: None,
            args: vec![],
            cwd: None,
            env_vars: Default::default(),
            description: None,
            authorization_env: None,
            allowed_tools: None,
            tool_catalog: Some(vec![McpToolCatalog {
                name: "list_issues".into(),
                description: None,
            }]),
            require_approval: McpApprovalMode::Always,
            reuse_connection: true,
        };
        let agent = agent(ToolRegistry::new(), vec![server.clone()]);
        let request = |tool: &str| McpApprovalRequest {
            approval_request_id: "req".into(),
            server_label: "github".into(),
            tool_name: tool.into(),
            arguments: json!({}),
        };

        let empty = ActiveTools::default();
        assert!(!with_scope(&empty, async |scope| agent
            .approve_responses_mcp_call(&request("list_issues"), scope)
            .await)
        .await
        .unwrap());

        let mut active = ActiveTools::default();
        active
            .responses_mcp
            .insert("github".into(), vec!["list_issues".into()]);
        assert!(with_scope(&active, async |scope| agent
            .approve_responses_mcp_call(&request("list_issues"), scope)
            .await)
        .await
        .unwrap());
        assert!(!with_scope(&active, async |scope| agent
            .approve_responses_mcp_call(&request("delete_issue"), scope)
            .await)
        .await
        .unwrap());

        // Concurrent approval requests reach the handler one at a time.
        let handler = Arc::new(CountingApproval {
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        });
        let agent = Agent::new(
            OpenAiClient::new("test", "http://127.0.0.1:1234/v1"),
            AgentSettings::default(),
            Arc::new(McpPool::new(vec![server])),
            ToolRegistry::new(),
            UserPolicy::default(),
            handler.clone(),
        );
        let items = (0..3)
            .map(|index| {
                json!({
                    "type": "mcp_approval_request",
                    "id": format!("req_{index}"),
                    "server_label": "github",
                    "name": "list_issues",
                    "arguments": "{}"
                })
            })
            .collect::<Vec<_>>();
        let (continuation, _) = with_scope(&active, async |scope| {
            agent.handle_output_items(&items, scope).await
        })
        .await
        .unwrap();
        assert_eq!(continuation.len(), 3);
        assert!(continuation.iter().all(|item| item["approve"] == true));
        assert_eq!(handler.max_in_flight.load(Ordering::SeqCst), 1);
    }
}
