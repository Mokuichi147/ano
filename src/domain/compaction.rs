//! Opaque standalone compaction: retain the provider's entire returned window.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
