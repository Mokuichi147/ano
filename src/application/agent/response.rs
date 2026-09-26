//! Parsing helpers for Responses API output items.

use crate::application::ports::McpApprovalRequest;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub(super) fn parse_arguments(value: &Value) -> Result<Value> {
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

pub(super) fn compact_output(value: &Value) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "{\"error\":\"failed to serialize tool output\"}".to_string())
}

pub(super) fn validate_response_status(response: &Value) -> Result<()> {
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

pub(super) fn extract_output_text(response: &Value) -> String {
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

/// The readable summary of a `reasoning` output item, if the provider sent one.
pub(super) fn reasoning_summary_text(item: &Value) -> Option<String> {
    if item["type"] != "reasoning" {
        return None;
    }
    let text = item["summary"]
        .as_array()?
        .iter()
        .filter(|part| part["type"] == "summary_text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.trim().is_empty()).then_some(text)
}

pub(super) fn parse_mcp_approval(item: &Value) -> Result<McpApprovalRequest> {
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
