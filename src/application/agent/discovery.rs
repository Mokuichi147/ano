//! Lazy tool discovery: `tool_search` selects the few tools whose schemas
//! are sent with the next request.

use super::{mcp_runtime::McpRuntime, Agent};
use crate::domain::{
    mcp::{McpServerConfig, McpTransport},
    plan::TASK_PLAN_NAME,
    policy::UserPolicy,
    tool::{ToolContext, ToolDefinition, DELEGATE_TASK_NAME, TOOL_SEARCH_NAME},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Tools loaded by the most recent `tool_search`.
#[derive(Default)]
pub(super) struct ActiveTools {
    pub local: BTreeSet<String>,
    /// Responses-managed MCP server label -> selected tool names.
    pub responses_mcp: BTreeMap<String, Vec<String>>,
    /// Function aliases of directly connected MCP tools.
    pub direct_mcp: BTreeSet<String>,
}

impl ActiveTools {
    /// The selection of the last `tool_search` in a conversation's history,
    /// so a new turn can still call the tools that earlier turns loaded.
    /// Without it the model sees its past calls but not the tools, and may
    /// announce a capability it can no longer call. Policy and availability
    /// are checked again when the request's tools are built.
    pub(super) fn from_history(history: &[Value]) -> Self {
        let searches: BTreeSet<&str> = history
            .iter()
            .filter(|item| item["type"] == "function_call" && item["name"] == TOOL_SEARCH_NAME)
            .filter_map(|item| item["call_id"].as_str())
            .collect();
        let Some(results) = history.iter().rev().find_map(|item| {
            if item["type"] != "function_call_output"
                || !item["call_id"]
                    .as_str()
                    .is_some_and(|id| searches.contains(id))
            {
                return None;
            }
            let output: Value = serde_json::from_str(item["output"].as_str()?).ok()?;
            output["tools"].as_array().cloned()
        }) else {
            return Self::default();
        };
        let mut active = Self::default();
        for result in results {
            let Some(name) = result["name"].as_str() else {
                continue;
            };
            match (
                result["server_label"].as_str(),
                result["function_name"].as_str(),
            ) {
                (Some(_), Some(function_name)) => {
                    active.direct_mcp.insert(function_name.to_string());
                }
                (Some(label), None) => active
                    .responses_mcp
                    .entry(label.to_string())
                    .or_default()
                    .push(name.to_string()),
                (None, _) => {
                    active.local.insert(name.to_string());
                }
            }
        }
        active
    }
}

impl Agent {
    /// The tools of the next request. `depth` is the nesting of the run;
    /// only the top-level run may delegate to a sub-agent.
    pub(super) fn response_tools(
        &self,
        active: &ActiveTools,
        mcp_runtime: &McpRuntime,
        depth: usize,
    ) -> Result<Vec<Value>> {
        let mut tools = Vec::new();
        if !self.policy.is_disabled(TOOL_SEARCH_NAME) {
            tools.push(tool_search_definition().as_response_tool());
        }
        if !self.policy.is_disabled(TASK_PLAN_NAME) {
            tools.push(task_plan_definition().as_response_tool());
        }
        if depth == 0 && !self.policy.is_disabled(DELEGATE_TASK_NAME) {
            tools.push(delegate_task_definition().as_response_tool());
        }

        tools.extend(
            self.registry
                .definitions(&self.policy)
                .into_iter()
                .filter(|definition| {
                    definition.always_offered || active.local.contains(&definition.name)
                })
                .map(|definition| definition.as_response_tool()),
        );

        for server in self.mcp.configs() {
            if server.transport != McpTransport::Responses {
                continue;
            }
            let Some(selected_tools) = active.responses_mcp.get(&server.label) else {
                continue;
            };
            if let Some(tool) = responses_mcp_tool(server, &self.policy, selected_tools)? {
                tools.push(tool);
            }
        }

        for (server, tool) in mcp_runtime.tools() {
            if !active.direct_mcp.contains(&tool.function_name) {
                continue;
            }
            let description = format!(
                "MCP tool '{}' on server '{}'. {}",
                tool.name,
                server.config().label,
                tool.description
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

    /// Tools that cannot run in `context` (see `ToolContext::can_run`) are
    /// left out of the results.
    pub(super) fn search_tools(
        &self,
        arguments: &Value,
        mcp_runtime: &McpRuntime,
        context: &ToolContext,
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
            if !context.can_run(&definition.name) {
                continue;
            }
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
                if let Some(score) = score_mcp_candidate(
                    &terms,
                    &[catalog.name.clone(), description.clone()],
                    server,
                ) {
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
            if let Some(score) = score_mcp_candidate(
                &terms,
                &[tool.name.clone(), tool.description.clone()],
                server.config(),
            ) {
                candidates.push(SearchCandidate {
                    kind: "mcp",
                    server_label: Some(server.config().label.clone()),
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
}

struct SearchCandidate {
    kind: &'static str,
    server_label: Option<String>,
    name: String,
    description: String,
    score: usize,
    function_name: Option<String>,
}

pub(super) struct ToolSearchSelection {
    pub query: String,
    pub results: Vec<Value>,
    pub active: ActiveTools,
}

fn tool_search_definition() -> ToolDefinition {
    ToolDefinition::new(
        TOOL_SEARCH_NAME,
        "Search the registered local tools and MCP tools by capability. Use this before attempting a tool that is not currently available; only the returned tools are loaded for the next step. Use short keywords; tool names and descriptions are usually in English.",
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

/// Score an MCP tool by its own name and description. The server's label
/// and description describe every tool on the server, so they only add to
/// the score of a tool that matches by itself, unless every term matches the
/// server (a query that names the server). Otherwise a word shared with the
/// server description would fill the results with all of its tools.
fn score_mcp_candidate(
    terms: &[String],
    tool_fields: &[String],
    server: &McpServerConfig,
) -> Option<usize> {
    let server_fields = [
        server.label.to_lowercase(),
        server
            .description
            .clone()
            .unwrap_or_default()
            .to_lowercase(),
    ];
    let server_matches = |term: &String| {
        server_fields
            .iter()
            .filter(|field| field.contains(term.as_str()))
            .count()
    };
    let server_score = terms.iter().map(server_matches).sum::<usize>();
    match score_candidate(terms, tool_fields) {
        Some(score) => Some(score + server_score),
        None if terms.iter().all(|term| server_matches(term) > 0) => Some(server_score),
        None => None,
    }
}

fn truncate_description(description: &str) -> String {
    let mut result = description.chars().take(240).collect::<String>();
    if description.chars().count() > 240 {
        result.push('…');
    }
    result
}

pub fn delegate_task_definition() -> ToolDefinition {
    ToolDefinition::new(
        DELEGATE_TASK_NAME,
        "Hand a self-contained task to a sub-agent that works in a fresh conversation with the same tools and permissions, and get back its final report. Use it for broad investigation (such as surveying many files) or independent parts of the work, so your own context stays focused. The sub-agent cannot see this conversation: state the goal, the relevant context, and what the report should contain. Several calls in one response run in parallel; do not give parallel sub-agents overlapping file edits. Sub-agents cannot delegate further.",
        json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "Complete instructions for the sub-agent"}
            },
            "required": ["task"],
            "additionalProperties": false
        }),
    )
}

pub fn task_plan_definition() -> ToolDefinition {
    ToolDefinition::new(TASK_PLAN_NAME,
        "Read or update this task's plan and goal. Use steps=null and goal=null to read. To update, send the current expected_revision (initially 0) with all steps and/or the whole goal; a null field keeps its current value. Use stable ids, at most one in_progress step, and detail for blocked reasons or completion evidence. Only the user sets a goal (its objective); do not create one yourself, and send goal=null unless the plan already has a goal. For a goal the user set, define concrete, checkable acceptance criteria that cover every condition its objective states; send objective=null, as the user's objective is always kept. Mark a criterion met only after verifying it (for example by running a check or reading the result), with the evidence; mark it blocked with the reason if it cannot be met. Changing or dropping steps or criteria requires an explanation. Plans persist with a session. Plan state is not proof that verification passed.",
        json!({"type":"object","properties":{
            "expected_revision":{"type":["integer","null"],"minimum":0},
            "explanation":{"type":["string","null"]},
            "steps":{"type":["array","null"],"minItems":1,"maxItems":50,"items":{
                "type":"object","properties":{
                    "id":{"type":"string"},"description":{"type":"string"},
                    "status":{"type":"string","enum":["pending","in_progress","completed","blocked"]},
                    "detail":{"type":["string","null"]}
                },"required":["id","description","status","detail"],"additionalProperties":false}},
            "goal":{"type":["object","null"],"properties":{
                "objective":{"type":["string","null"]},
                "acceptance":{"type":"array","minItems":1,"maxItems":20,"items":{
                    "type":"object","properties":{
                        "id":{"type":"string"},"description":{"type":"string"},
                        "status":{"type":"string","enum":["pending","met","blocked"]},
                        "evidence":{"type":["string","null"]}
                    },"required":["id","description","status","evidence"],"additionalProperties":false}}
            },"required":["objective","acceptance"],"additionalProperties":false}
        },"required":["expected_revision","explanation","steps","goal"],"additionalProperties":false}))
}

/// Build the Responses API `mcp` tool that exposes only `selected_tools`.
///
/// Returns `None` when nothing selected is allowed for the policy, so a
/// server is never delegated to the provider without a function filter.
pub(super) fn responses_mcp_tool(
    server: &McpServerConfig,
    policy: &UserPolicy,
    selected_tools: &[String],
) -> Result<Option<Value>> {
    if server.transport != McpTransport::Responses {
        bail!(
            "MCP server '{}' uses a direct transport and cannot be delegated to the Responses API",
            server.label
        );
    }
    server.validate()?;

    let filtered: Vec<&String> = selected_tools
        .iter()
        .filter(|name| server.is_tool_allowed(policy, name))
        .collect();
    if filtered.is_empty() {
        return Ok(None);
    }

    let mut tool = json!({
        "type": "mcp",
        "server_label": server.label,
        "require_approval": server.require_approval.as_str(),
        "allowed_tools": filtered,
    });

    if let Some(url) = &server.url {
        tool["server_url"] = json!(url);
    }
    if let Some(tunnel_id) = &server.tunnel_id {
        tool["tunnel_id"] = json!(tunnel_id);
    }
    if let Some(description) = &server.description {
        tool["server_description"] = json!(description);
    }

    if let Some(authorization_env) = &server.authorization_env {
        let authorization = std::env::var(authorization_env).with_context(|| {
            format!(
                "MCP server '{}' requires environment variable '{}'",
                server.label, authorization_env
            )
        })?;
        tool["authorization"] = json!(authorization);
    }

    Ok(Some(tool))
}
