//! Reading the answers of the judging models (the approval reviewer and
//! triage), which are asked for a JSON object but may wrap it in prose or a
//! code fence, as endpoints without structured output do.

use anyhow::{bail, Result};
use serde_json::Value;

/// The text of a Responses API response, or its error.
pub(crate) fn response_text(response: &Value) -> Result<String> {
    if let Some(error) = response["error"]["message"].as_str() {
        bail!("the model returned an error: {error}");
    }
    Ok(response["output_text"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| {
            response["output"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "message")
                .flat_map(|item| item["content"].as_array().into_iter().flatten())
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        }))
}

/// The outermost JSON object in `text`, when it parses.
pub(crate) fn json_object(text: &str) -> Option<Value> {
    let (start, end) = (text.find('{')?, text.rfind('}')?);
    serde_json::from_str::<Value>(&text[start..=end.max(start)])
        .ok()
        .filter(Value::is_object)
}

/// A short excerpt of an answer that could not be read, for errors.
pub(crate) fn unreadable(text: &str, what: &str) -> String {
    let answer = text.trim();
    if answer.is_empty() {
        format!("{what} gave an empty answer")
    } else {
        let excerpt: String = answer.chars().take(200).collect();
        format!("could not read the {what} answer {excerpt:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn objects_are_found_inside_prose_and_fences() {
        let text = "Sure:\n```json\n{\"action\": \"notify\"}\n```";
        assert_eq!(json_object(text), Some(json!({"action": "notify"})));
        assert_eq!(json_object("no object"), None);
        assert_eq!(json_object("{broken"), None);
    }

    #[test]
    fn text_comes_from_output_text_or_messages_and_errors_fail() {
        assert_eq!(response_text(&json!({"output_text": "a"})).unwrap(), "a");
        let message = json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": "b"}]}]});
        assert_eq!(response_text(&message).unwrap(), "b");
        assert!(response_text(&json!({"error": {"message": "down"}})).is_err());
    }
}
