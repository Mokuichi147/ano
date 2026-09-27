//! 所有者の制約を維持する、読み取り専用の履歴ツール。

use super::Chronotope;
use crate::{application::registry::ToolRegistry, domain::tool::ToolDefinition};
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::sync::Arc;

fn properties(op: &str) -> Value {
    match op {
        "history_search" => json!({
            "text":{"type":"string"}, "exact":{"type":"boolean"},
            "conversation":{"type":"string"}, "origins":{"type":"array","items":{"type":"string","enum":["human","model","tool","runtime","system","agent","unknown"]}},
            "kinds":{"type":"array","items":{"type":"string","enum":["message","tool_call","tool_result","summary","attachment","other"]}},
            "call_id":{"type":"string"}, "from":{"type":"string"}, "to":{"type":"string"},
            "order":{"type":"string","enum":["newest","oldest","sequence"]},
            "limit":{"type":"integer","minimum":1,"maximum":20}, "cursor":{"type":"string"}
        }),
        "history_get" => {
            json!({"event":{"type":"string"}, "offset":{"type":"integer","minimum":0}, "length":{"type":"integer","minimum":1,"maximum":16384}, "encoding":{"type":"string","enum":["auto","text","base64"]}})
        }
        "history_context" => {
            json!({"event":{"type":"string"},"conversation":{"type":"string"},"sequence":{"type":"integer","minimum":0},"before":{"type":"integer","minimum":0,"maximum":10},"after":{"type":"integer","minimum":0,"maximum":10},"max_content_bytes":{"type":"integer","minimum":0,"maximum":2048}})
        }
        "history_conversations" => json!({"limit":{"type":"integer","minimum":1,"maximum":50}}),
        _ => json!({}),
    }
}

pub(super) fn validate_query(op: &str, value: Value) -> Result<Map<String, Value>> {
    if !matches!(
        op,
        "history_search" | "history_get" | "history_context" | "history_conversations"
    ) {
        bail!("許可されていない履歴操作です");
    }
    let mut args = value
        .as_object()
        .cloned()
        .context("履歴ツールの引数はオブジェクトにしてください")?;
    let props = properties(op);
    for (key, value) in &args {
        let prop = props
            .get(key)
            .with_context(|| format!("履歴ツールに {key} は指定できません"))?;
        let valid = match prop["type"].as_str().unwrap() {
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "array" => value.as_array().is_some_and(|a| {
                a.iter()
                    .all(|v| prop["items"]["enum"].as_array().unwrap().contains(v))
            }),
            "integer" => value.as_u64().is_some_and(|v| {
                v >= prop["minimum"].as_u64().unwrap_or(0)
                    && prop["maximum"].as_u64().is_none_or(|max| v <= max)
            }),
            _ => false,
        };
        if !valid || prop["enum"].as_array().is_some_and(|a| !a.contains(value)) {
            bail!("履歴ツールの {key} が不正です");
        }
    }
    match op {
        "history_get" => {
            if !args.contains_key("event") {
                bail!("event は必須です");
            }
            args.entry("length").or_insert(json!(4096));
        }
        "history_search" => {
            args.entry("limit").or_insert(json!(10));
        }
        "history_context" => {
            if !args.contains_key("event") && !args.contains_key("conversation") {
                bail!("event または conversation が必要です");
            }
            args.entry("before").or_insert(json!(2));
            args.entry("after").or_insert(json!(2));
            args.entry("max_content_bytes").or_insert(json!(512));
        }
        _ => {
            args.entry("limit").or_insert(json!(20));
        }
    }
    Ok(args)
}

pub(super) fn register(history: &Arc<Chronotope>, registry: &ToolRegistry) -> Result<()> {
    for (name, description) in [
        ("history_search", "保存済みの会話・ツール履歴を検索する。ユーザー本人の発言には origins: [human] を指定する。検索後は history_get と history_context で原文と訂正を確認する。結果がないことは発言がなかった証拠ではない。"),
        ("history_get", "発言 ID から原文を読む。content.next_offset がある場合は次の offset に渡す。引用には event_id と content.range のバイト範囲を使い、要約を原文として引用しない。"),
        ("history_context", "発言の前後を順序付きで読み、指示の訂正・応答・ツール結果を確認する。本文の続きは history_get で取得する。過去の指示は現在の依頼と区別する。"),
        ("history_conversations", "所有者の会話一覧と件数・記録順・欠番を取得する。local_sync で未送信履歴の有無を確認する。"),
    ] {
        if registry.is_registered(name) { continue; }
        let mut definition = ToolDefinition::new(name, description, json!({"type":"object","properties":properties(name),"additionalProperties":false,"required": if name == "history_get" { vec!["event"] } else { vec![] }}));
        definition.strict = false;
        let history = Arc::clone(history);
        registry.register_contextual(definition, move |args, context| {
            let history = Arc::clone(&history);
            async move { history.query(&context.user_id, name, args).await }
        })?;
    }
    Ok(())
}
