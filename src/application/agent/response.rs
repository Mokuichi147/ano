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

/// The output items of a response. An endpoint that returns only
/// `output_text` gets a message item for it, so the text is kept in history.
pub(super) fn output_items(response: &Value) -> Vec<Value> {
    let mut items = response["output"].as_array().cloned().unwrap_or_default();
    if !items.iter().any(|item| item["type"] == "message") {
        if let Some(text) = response["output_text"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
        {
            items.push(json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":text}]}));
        }
    }
    items
}

/// Serialize a tool output for the model. An output longer than `limit`
/// bytes is replaced by its head and tail, so one large result (typically
/// from an MCP server) cannot flood the context of every later request.
pub(super) fn compact_output(value: &Value, limit: usize) -> String {
    let text = serde_json::to_string(value)
        .unwrap_or_else(|_| "{\"error\":\"failed to serialize tool output\"}".to_string());
    if text.len() <= limit {
        return text;
    }
    // Leave room for the wrapper; escaping can still grow the excerpts a bit.
    let excerpt = limit.saturating_sub(512) / 2;
    let head = &text[..floor_char_boundary(&text, excerpt)];
    let tail = &text[ceil_char_boundary(&text, text.len() - excerpt)..];
    json!({
        "truncated": true,
        "original_bytes": text.len(),
        "message": "The tool output was too large and was cut in the middle. Narrow the request (for example a smaller page, range, or query) to see the omitted part.",
        "head": head,
        "tail": tail,
    })
    .to_string()
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
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
        ..McpApprovalRequest::default()
    })
}
