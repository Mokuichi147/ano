//! How the values of events read in the timeline.

use serde_json::Value;

/// A field as the server sent it, which cuts long ones to a preview.
fn preview(value: &Value) -> Option<&str> {
    (value["truncated"] == true)
        .then(|| value["preview"].as_str())
        .flatten()
}

/// `value` for reading in full: indented JSON, or the text itself.
pub fn pretty(value: &Value) -> String {
    if let Some(preview) = preview(value) {
        return format!("{preview}…（省略）");
    }
    if let Some(text) = value.as_str() {
        return match serde_json::from_str::<Value>(text) {
            Ok(parsed) if parsed.is_object() || parsed.is_array() => {
                serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| text.to_string())
            }
            _ => text.to_string(),
        };
    }
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// `value` on one line, cut after `limit` characters.
pub fn one_line(value: &Value, limit: usize) -> String {
    let text = match (preview(value), value.as_str()) {
        (Some(preview), _) => preview.to_string(),
        (None, Some(text)) => text.to_string(),
        (None, None) => value.to_string(),
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > limit {
        format!("{}…", text.chars().take(limit).collect::<String>())
    } else {
        text
    }
}

/// The approval modes, in the order the form offers them, with their labels.
pub const APPROVAL_MODES: [(&str, &str); 4] = [
    ("ask", "手動（ask）"),
    ("auto", "自動（auto）"),
    ("allow", "全て許可（allow）"),
    ("deny", "全て拒否（deny）"),
];

/// The label of approval mode `mode`.
pub fn approval_mode(mode: &str) -> &str {
    APPROVAL_MODES
        .iter()
        .find(|(name, _)| *name == mode)
        .map_or(mode, |(_, label)| label)
}

pub fn outcome(outcome: &str) -> &'static str {
    match outcome {
        "completed" => "完了",
        "blocked" => "行き詰まり",
        "incomplete" => "未完了",
        _ => "—",
    }
}

/// Why a turn stopped, when it was not a final answer.
pub fn stop_reason(reason: &str) -> Option<&'static str> {
    match reason {
        "round_limit" => Some("実行回数の上限に達しました"),
        "token_limit" => Some("トークンの上限に達しました"),
        "usage_unavailable" => Some("使用量が取得できず停止しました"),
        "no_progress" => Some("同じ呼び出しが続いたため停止しました"),
        _ => None,
    }
}

/// The mark of a plan step or acceptance criterion.
pub fn mark(status: &str) -> &'static str {
    match status {
        "in_progress" => "▶",
        "completed" | "met" => "✓",
        "blocked" => "!",
        _ => "○",
    }
}

pub fn usage(usage: &Value) -> String {
    let number = |field: &str| usage[field].as_u64().unwrap_or(0);
    format!(
        "{} トークン（入力 {}・出力 {}）・応答 {} 回",
        number("total_tokens"),
        number("input_tokens"),
        number("output_tokens"),
        number("responses")
    )
}
