//! History compaction, in one of two forms:
//!
//! - remote: the provider's `/responses/compact` returns an opaque window,
//!   which is retained as a whole;
//! - summary: for endpoints without that API, the model summarizes a text
//!   transcript of the earlier history, and the summary replaces it; the
//!   most recent steps are kept as they are, so the work in progress, such
//!   as a file just read, need not be read again.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How the history is compacted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMethod {
    /// `remote` when the endpoint supports `/responses/compact`, otherwise
    /// `summary`.
    #[default]
    Auto,
    Remote,
    Summary,
}

/// Starts the user message part that carries a summary, so that a later
/// compaction recognizes it and does not keep it as the user's own words.
pub const SUMMARY_NOTICE: &str = "Runtime notice: the earlier conversation was compacted to save context, and this summary replaces it.";

/// Upper bounds, in characters, of one entry of a summary transcript.
const TRANSCRIPT_TEXT_CHARS: usize = 6000;
const TRANSCRIPT_TOOL_CHARS: usize = 1500;
/// The latest user message is kept verbatim only up to this size.
const MAX_RETAINED_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionRecord {
    pub id: String,
    pub before_bytes: usize,
    pub after_bytes: usize,
    pub before_items: usize,
    pub after_items: usize,
    /// A sibling of the session file, created before its history is replaced.
    pub archive_file: Option<String>,
}

pub(crate) fn history_bytes(history: &[Value]) -> Result<usize> {
    Ok(serde_json::to_vec(history)?.len())
}

pub(crate) fn compaction_due(
    history: &[Value],
    threshold: Option<usize>,
    previous_size: Option<usize>,
) -> Result<bool> {
    let Some(threshold) = threshold else {
        return Ok(false);
    };
    // Opaque ciphertext may occupy more bytes than the original text. Require
    // new history growth rather than repeatedly compacting the same window.
    let trigger = previous_size
        .map(|size| size.saturating_add(threshold / 2).max(threshold))
        .unwrap_or(threshold);
    Ok(history_bytes(history)? >= trigger)
}

pub(crate) fn compacted_history(
    response: &Value,
    previous: &[Value],
) -> Result<(Vec<Value>, CompactionRecord)> {
    if response["object"] != "response.compaction" || !response["error"].is_null() {
        bail!("invalid compaction response; original history was preserved");
    }
    let id = response["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("compaction response has no id")?;
    let output = response["output"]
        .as_array()
        .context("compaction response has no output array")?;
    if !output.iter().all(Value::is_object)
        || !output.iter().any(|item| {
            item["type"] == "compaction"
                && item["encrypted_content"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
        })
    {
        bail!("compaction response has no valid encrypted compaction item; original history was preserved");
    }
    let record = CompactionRecord {
        id: id.into(),
        before_bytes: history_bytes(previous)?,
        after_bytes: history_bytes(output)?,
        before_items: previous.len(),
        after_items: output.len(),
        archive_file: None,
    };
    Ok((output.clone(), record))
}

/// One rendered history item of a summary transcript.
struct Entry {
    text: String,
    /// Kept even when the transcript is over its budget: earlier summaries
    /// and the latest request.
    pinned: bool,
}

/// Render `history` as a plain-text transcript of at most about `limit`
/// bytes for the model to summarize. Tool calls and results are shortened;
/// when the transcript is still too long, the oldest unpinned entries are
/// left out.
pub fn summary_transcript(history: &[Value], limit: usize) -> String {
    let last_request = last_user_request(history).map(|(index, _)| index);
    let mut entries = Vec::new();
    for (index, item) in history.iter().enumerate() {
        let role = item["role"].as_str().unwrap_or_default();
        let kind = item["type"]
            .as_str()
            .unwrap_or(if role.is_empty() { "" } else { "message" });
        let entry = match kind {
            "message" => {
                let parts = message_parts(item);
                let mut texts = Vec::new();
                for part in parts {
                    if let Some(summary) = part.strip_prefix(SUMMARY_NOTICE) {
                        entries.push(Entry {
                            text: format!("## Summary of earlier conversation\n{}", summary.trim()),
                            pinned: true,
                        });
                    } else {
                        texts.push(part);
                    }
                }
                if texts.is_empty() {
                    continue;
                }
                let heading = if role == "user" { "User" } else { "Assistant" };
                Entry {
                    text: format!(
                        "## {heading}\n{}",
                        shorten(&texts.join("\n"), TRANSCRIPT_TEXT_CHARS)
                    ),
                    pinned: Some(index) == last_request,
                }
            }
            "function_call" => Entry {
                text: format!(
                    "## Tool call: {}\n{}",
                    item["name"].as_str().unwrap_or("unknown"),
                    shorten(&value_text(&item["arguments"]), TRANSCRIPT_TOOL_CHARS)
                ),
                pinned: false,
            },
            "function_call_output" => Entry {
                text: format!(
                    "## Tool result\n{}",
                    shorten(&value_text(&item["output"]), TRANSCRIPT_TOOL_CHARS)
                ),
                pinned: false,
            },
            "mcp_call" => Entry {
                text: format!(
                    "## MCP call: {}:{}\n{}\n-> {}",
                    item["server_label"].as_str().unwrap_or("unknown"),
                    item["name"].as_str().unwrap_or("unknown"),
                    shorten(&value_text(&item["arguments"]), TRANSCRIPT_TOOL_CHARS),
                    shorten(
                        &value_text(if item["error"].is_null() {
                            &item["output"]
                        } else {
                            &item["error"]
                        }),
                        TRANSCRIPT_TOOL_CHARS
                    )
                ),
                pinned: false,
            },
            _ => continue,
        };
        entries.push(entry);
    }

    let mut keep = vec![false; entries.len()];
    let mut used = 0;
    for (index, entry) in entries.iter().enumerate() {
        if entry.pinned {
            keep[index] = true;
            used += entry.text.len() + 2;
        }
    }
    for (index, entry) in entries.iter().enumerate().rev() {
        if keep[index] {
            continue;
        }
        if used + entry.text.len() + 2 > limit {
            break;
        }
        keep[index] = true;
        used += entry.text.len() + 2;
    }
    let mut output = Vec::new();
    let mut omitted = 0;
    for (entry, keep) in entries.iter().zip(keep) {
        if keep {
            if omitted > 0 {
                output.push(format!("[{omitted} earlier item(s) omitted]"));
                omitted = 0;
            }
            output.push(entry.text.clone());
        } else {
            omitted += 1;
        }
    }
    if omitted > 0 {
        output.push(format!("[{omitted} item(s) omitted]"));
    }
    output.join("\n\n")
}

/// Replace `previous` with the latest user request and `summary` of the rest.
///
/// The result is one user message: the request's own parts followed by the
/// summary, so that endpoints whose chat templates need alternating roles
/// accept it.
/// Where the recent part of `history` that a summary keeps as it is begins:
/// at most `budget` bytes, starting where a response starts (not at a tool
/// result, which must follow its call). `history.len()` when no such part
/// fits; the first item is always summarized.
pub fn summary_split(history: &[Value], budget: usize) -> usize {
    let is_result = |item: &Value| {
        matches!(
            item["type"].as_str(),
            Some("function_call_output" | "mcp_approval_response")
        )
    };
    let mut split = history.len();
    let mut kept = 0;
    for index in (1..history.len()).rev() {
        kept += serde_json::to_vec(&history[index]).map_or(0, |bytes| bytes.len());
        if kept > budget {
            break;
        }
        let before = &history[index - 1];
        let item = &history[index];
        if !is_result(item)
            && item["role"] != "user"
            && (is_result(before) || before["role"] == "user")
        {
            split = index;
        }
    }
    split
}

/// Replace `previous[..split]` with `summary`, keeping `previous[split..]`
/// (see `summary_split`) after it.
pub fn summarized_history(
    previous: &[Value],
    split: usize,
    summary: &str,
    id: String,
) -> Result<(Vec<Value>, CompactionRecord)> {
    let summary = summary.trim();
    if summary.is_empty() {
        bail!("the model returned an empty summary; original history was preserved");
    }
    let split = split.min(previous.len());
    // The request is repeated only when the summary replaces it.
    let mut content = match last_user_request(previous).filter(|(index, _)| *index < split) {
        Some((_, parts)) if history_bytes(&parts)? <= MAX_RETAINED_REQUEST_BYTES => parts,
        Some((index, _)) => vec![json!({
            "type": "input_text",
            "text": shorten(&message_parts(&previous[index]).join("\n"), TRANSCRIPT_TEXT_CHARS),
        })],
        None => Vec::new(),
    };
    let kept = &previous[split..];
    let recent = if kept.is_empty() {
        ""
    } else {
        " The most recent steps follow it unchanged."
    };
    content.push(json!({
        "type": "input_text",
        "text": format!("{SUMMARY_NOTICE}{recent} Continue the work from it; reread files before relying on details the summary gives.\n\n{summary}"),
    }));
    let mut history = vec![json!({"role": "user", "content": content})];
    history.extend_from_slice(kept);
    let record = CompactionRecord {
        id,
        before_bytes: history_bytes(previous)?,
        after_bytes: history_bytes(&history)?,
        before_items: previous.len(),
        after_items: history.len(),
        archive_file: None,
    };
    Ok((history, record))
}

/// The index and content parts of the user's latest own message, without
/// runtime notices and earlier summaries.
fn last_user_request(history: &[Value]) -> Option<(usize, Vec<Value>)> {
    history.iter().enumerate().rev().find_map(|(index, item)| {
        if item["role"] != "user" {
            return None;
        }
        let parts = match &item["content"] {
            Value::String(text) => vec![json!({"type": "input_text", "text": text})],
            Value::Array(parts) => parts.clone(),
            _ => return None,
        };
        let own = parts
            .into_iter()
            .filter(|part| {
                !part["text"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("Runtime notice"))
            })
            .collect::<Vec<_>>();
        (!own.is_empty()).then_some((index, own))
    })
}

/// The readable parts of a message; images, audio, and files are named.
fn message_parts(item: &Value) -> Vec<String> {
    match &item["content"] {
        Value::String(text) => vec![text.clone()],
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part["type"].as_str().unwrap_or_default() {
                "input_image" => "[image]".to_string(),
                "input_audio" => "[audio]".to_string(),
                "input_file" => "[file]".to_string(),
                _ => part["text"]
                    .as_str()
                    .or_else(|| part["refusal"].as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
            .filter(|text| !text.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

/// `text` cut to its first and last characters when longer than `limit`.
fn shorten(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let half = limit / 2;
    let head = text.chars().take(half).collect::<String>();
    let tail = text.chars().skip(count - half).collect::<String>();
    format!(
        "{head}\n[... {} characters omitted ...]\n{tail}",
        count - 2 * half
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_all_compacted_items_without_decoding_or_pruning() {
        let output = vec![
            json!({"role":"user","content":"keep the constraints"}),
            json!({"type":"compaction","encrypted_content":"opaque"}),
            json!({"type":"message","role":"assistant","content":[]}),
        ];
        let (retained, _) = compacted_history(
            &json!({"object":"response.compaction","id":"cmp1","output":output}),
            &[],
        )
        .unwrap();
        assert_eq!(retained, output);
        assert!(compacted_history(
            &json!({"object":"response.compaction","id":"empty","output":[]}),
            &[]
        )
        .is_err());
    }

    #[test]
    fn compaction_requires_growth_after_a_pass() {
        let history = vec![json!({"content":"x".repeat(2048)})];
        let size = history_bytes(&history).unwrap();
        assert!(compaction_due(&history, Some(1024), None).unwrap());
        assert!(!compaction_due(&history, Some(1024), Some(size)).unwrap());
        assert!(!compaction_due(&history, None, None).unwrap());
    }

    fn user(text: &str) -> Value {
        json!({"role":"user","content":[{"type":"input_text","text":text}]})
    }

    #[test]
    fn summary_keeps_the_latest_request_and_replaces_the_rest() {
        let history = vec![
            user("最初の依頼"),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"了解"}]}),
            user("README を直して"),
            json!({"type":"reasoning","encrypted_content":"opaque"}),
            json!({"type":"function_call","call_id":"c1","name":"workspace_read","arguments":"{\"path\":\"README.md\"}"}),
            json!({"type":"function_call_output","call_id":"c1","output":"x".repeat(10_000)}),
            user("Runtime notice: your recorded task plan still has pending steps."),
        ];
        let transcript = summary_transcript(&history, 100_000);
        assert!(transcript.contains("## User\n最初の依頼"));
        assert!(transcript.contains("## Tool call: workspace_read"));
        assert!(transcript.contains("characters omitted"));
        assert!(!transcript.contains("opaque"));

        let everything = history.len();
        let (compacted, record) = summarized_history(
            &history,
            everything,
            "README の2節を修正済み",
            "local-1".into(),
        )
        .unwrap();
        assert_eq!(compacted.len(), 1);
        let parts = compacted[0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["text"], "README を直して");
        let notice = parts[1]["text"].as_str().unwrap();
        assert!(notice.starts_with(SUMMARY_NOTICE) && notice.ends_with("README の2節を修正済み"));
        assert_eq!((record.before_items, record.after_items), (7, 1));
        assert!(record.after_bytes < record.before_bytes);
        assert!(summarized_history(&history, everything, "  ", "local-2".into()).is_err());

        // A second compaction carries the summary over as a summary, and the
        // request is not duplicated.
        let mut next = compacted.clone();
        next.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"続けます"}]}));
        let transcript = summary_transcript(&next, 100_000);
        assert!(transcript.contains("## Summary of earlier conversation\nContinue the work"));
        assert_eq!(transcript.matches("README を直して").count(), 1);
        let (again, _) =
            summarized_history(&next, next.len(), "新しい要約", "local-3".into()).unwrap();
        let parts = again[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "README を直して");
    }

    #[test]
    fn a_summary_keeps_the_most_recent_steps_as_they_are() {
        let read = |id: &str, bytes: usize| {
            [
                json!({"type":"reasoning","encrypted_content":"opaque"}),
                json!({"type":"function_call","call_id":id,"name":"workspace_read","arguments":"{}"}),
                json!({"type":"function_call_output","call_id":id,"output":"z".repeat(bytes)}),
            ]
        };
        let mut history = vec![user("README を直して")];
        history.extend(read("c1", 5000));
        history.extend(read("c2", 5000));
        history.extend(read("c3", 500));
        // The last response and its result fit; a result alone is never kept.
        let split = summary_split(&history, 2000);
        assert_eq!(split, 7);
        assert_eq!(history[split]["type"], "reasoning");
        assert_eq!(summary_split(&history, 100), history.len());
        let split_before_last = summary_split(&history, 7000);
        assert_eq!(split_before_last, 4);

        let (compacted, record) =
            summarized_history(&history, split, "c1 と c2 を読んだ", "local-4".into()).unwrap();
        assert_eq!(compacted.len(), 4);
        assert_eq!(&compacted[1..], &history[7..]);
        let parts = compacted[0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["text"], "README を直して");
        assert!(parts[1]["text"]
            .as_str()
            .unwrap()
            .contains("The most recent steps follow it unchanged."));
        assert_eq!((record.before_items, record.after_items), (10, 4));

        // A request among the kept steps is not repeated in the summary.
        let mut later = history.clone();
        later.push(user("続けて CHANGELOG も"));
        later.extend(read("c4", 100));
        let split = summary_split(&later, 3000);
        assert_eq!(split, 7);
        let (compacted, _) = summarized_history(&later, split, "要約", "local-5".into()).unwrap();
        let parts = compacted[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn over_budget_transcripts_keep_pinned_and_recent_entries() {
        let mut history = vec![user("依頼")];
        for index in 0..50 {
            history.push(json!({"type":"function_call_output","call_id":format!("c{index}"),"output":format!("result {index} {}", "y".repeat(500))}));
        }
        let transcript = summary_transcript(&history, 3000);
        assert!(transcript.len() < 3500);
        assert!(transcript.starts_with("## User\n依頼"));
        assert!(transcript.contains("earlier item(s) omitted"));
        assert!(transcript.contains("result 49"));
        assert!(!transcript.contains("result 0 "));
    }
}
