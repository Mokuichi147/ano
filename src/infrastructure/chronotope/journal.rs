//! 原文イベントの追記保存と、実行用ストアへの記録フック。

use super::{event_path, lock_file, next_sequence, private_dir, sync_dir};
use crate::{
    application::ports::ConversationStore,
    domain::{
        compaction::CompactionRecord,
        plan::TaskPlan,
        session::{ModelChoice, SessionData, SessionStatus},
        usage::UsageSummary,
    },
    infrastructure::fs::atomic_write,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs::File, path::PathBuf};
use uuid::Uuid;

pub(super) struct RecordedConversation<'a> {
    inner: &'a mut dyn ConversationStore,
    dir: PathBuf,
    _lock: File,
    next: u64,
    turn: String,
    parent: Value,
}

impl<'a> RecordedConversation<'a> {
    pub fn open(root: PathBuf, inner: &'a mut dyn ConversationStore) -> Result<Self> {
        let id = &inner.data().conversation_id;
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            bail!("履歴の会話 ID が不正です");
        }
        let dir = root.join(id);
        private_dir(&dir)?;
        let lock = lock_file(&dir.join("journal.lock"))?;
        lock.try_lock()
            .context("同じ会話の原文履歴を別の実行が使用中です")?;
        let manifest = json!({"version":1,"conversation":id,"binding":inner.data().binding});
        let manifest_path = dir.join("manifest.json");
        if std::fs::symlink_metadata(&manifest_path)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            bail!("原文履歴の manifest はシンボリックリンクにできません");
        }
        if manifest_path.exists() {
            let stored: Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
            // 会話の途中でモデルの接続先を切り替えられるため、接続先は照合しない。
            if without_endpoint(stored) != without_endpoint(manifest.clone()) {
                bail!("原文履歴は別のユーザー・環境に属しています");
            }
        } else {
            atomic_write(&manifest_path, &serde_json::to_vec(&manifest)?, None)?;
            sync_dir(&dir)?;
        }
        let next = next_sequence(&dir)?;
        let mut this = Self {
            inner,
            dir,
            _lock: lock,
            next,
            turn: Uuid::new_v4().to_string(),
            parent: Value::Null,
        };
        let active_path = this.dir.join("active.json");
        if std::fs::symlink_metadata(&active_path)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            bail!("原文履歴の active はシンボリックリンクにできません");
        }
        if active_path.exists() {
            this.notice("前の記録処理が中断されました。ツールの副作用は不明です。過去のツールは再実行していません。", "unknown")?;
            this.clear_active()?;
        }
        Ok(this)
    }

    fn append(&mut self, mut event: Value) -> Result<()> {
        event["event_id"] = json!(Uuid::new_v4().to_string());
        event["conversation"] = json!(self.inner.data().conversation_id);
        event["sequence"] = json!(self.next);
        event["turn"] = json!(self.turn);
        event["received_at"] = json!(timestamp());
        if !event["metadata"].is_object() {
            event["metadata"] = json!({});
        }
        event["metadata"]["environment"] = json!(self.inner.data().binding.environment);
        if !self.parent.is_null() {
            event["metadata"]["delegation"] = self.parent.clone();
        }
        let next = self
            .next
            .checked_add(1)
            .context("履歴の記録順が上限に達しました")?;
        let path = event_path(&self.dir, self.next);
        if path.exists() {
            bail!("履歴の記録順が重複しています");
        }
        atomic_write(&path, &serde_json::to_vec(&event)?, None)?;
        sync_dir(&self.dir)?;
        self.next = next;
        Ok(())
    }

    /// 大きな原文は文字境界で分割し、全体のハッシュとバイト位置を各片に残す。
    fn text(&mut self, event: Value, text: &str) -> Result<()> {
        const CHUNK: usize = 256 * 1024;
        if text.len() <= CHUNK {
            let mut event = event;
            event["content"] = json!(text);
            return self.append(event);
        }
        let group = Uuid::new_v4().to_string();
        let hash = hex::encode(Sha256::digest(text.as_bytes()));
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + CHUNK).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let mut part = event.clone();
            if !part["metadata"].is_object() {
                part["metadata"] = json!({});
            }
            part["metadata"]["content_group"] = json!(group);
            part["metadata"]["content_range"] = json!({"unit":"byte","start":start,"end":end,"total_size":text.len(),"sha256":hash});
            part["content"] = json!(&text[start..end]);
            self.append(part)?;
            start = end;
        }
        Ok(())
    }

    fn binary(&mut self, event: Value, data: &str) -> Result<()> {
        let bytes = STANDARD.decode(data).context("添付の base64 が不正です")?;
        let group = Uuid::new_v4().to_string();
        let hash = hex::encode(Sha256::digest(&bytes));
        let chunks = bytes.chunks(256 * 1024).collect::<Vec<_>>();
        for (index, chunk) in chunks.iter().enumerate() {
            let mut part = event.clone();
            part["content_base64"] = json!(STANDARD.encode(chunk));
            if !part["metadata"].is_object() {
                part["metadata"] = json!({});
            }
            part["metadata"]["content_group"] = json!(group);
            part["metadata"]["content_range"] = json!({"unit":"byte","start":index * 256 * 1024,"end":index * 256 * 1024 + chunk.len(),"total_size":bytes.len(),"sha256":hash});
            self.append(part)?;
        }
        if chunks.is_empty() {
            let mut event = event;
            event["content_base64"] = json!("");
            self.append(event)?;
        }
        Ok(())
    }

    fn items(&mut self, items: &Value, origin: &str, response_id: Option<&str>) -> Result<()> {
        for item in items.as_array().context("履歴の入力は配列にしてください")? {
            self.item(item, origin, response_id)?;
        }
        Ok(())
    }

    fn item(&mut self, item: &Value, origin: &str, response_id: Option<&str>) -> Result<()> {
        if is_reasoning(item) {
            return Ok(());
        }
        let kind = item["type"].as_str().unwrap_or("message");
        let mut event = json!({"kind":"other","origin":origin,"metadata":{"item_type":kind}});
        if let Some(id) = response_id {
            event["response_id"] = json!(id);
        }
        if let Some(role) = item.get("role") {
            event["api_role"] = role.clone();
        }
        if let Some(phase) = item.get("phase") {
            event["metadata"]["phase"] = phase.clone();
        }
        if let Some(id) = item.get("id") {
            event["metadata"]["api_item_id"] = id.clone();
        }
        match kind {
            // サーバー側で実行された MCP 呼び出しは、引数と結果を 1 項目で返す。
            "mcp_call" => {
                let mut call = item.clone();
                call["type"] = json!("function_call");
                self.item(&call, origin, response_id)?;
                let mut result = item.clone();
                result["type"] = json!("function_call_output");
                if result["output"].is_null() && !item["error"].is_null() {
                    result["output"] = json!({"error": item["error"]});
                }
                self.item(&result, origin, response_id)
            }
            "function_call" | "function_call_output" => {
                let result = kind != "function_call";
                event["kind"] = json!(if result { "tool_result" } else { "tool_call" });
                if result && origin == "model" {
                    event["origin"] = json!("tool");
                }
                if let Some(id) = item.get("call_id").or_else(|| item.get("id")) {
                    event["call_id"] = id.clone();
                }
                if let Some(name) = item.get("name") {
                    event["metadata"]["tool_name"] = name.clone();
                }
                if let Some(label) = item.get("server_label") {
                    event["metadata"]["mcp_server"] = label.clone();
                }
                let content = &item[if result { "output" } else { "arguments" }];
                let text = content
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| content.to_string());
                let parsed = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
                let status = if !result || parsed["error"] == "execution_interrupted" {
                    "unknown"
                } else if parsed.get("error").is_some_and(|v| !v.is_null())
                    || parsed["isError"] == true
                    || parsed["is_error"] == true
                    || parsed["success"] == false
                {
                    "error"
                } else {
                    "ok"
                };
                event["status"] = json!(status);
                self.text(event, &text)
            }
            "message" => {
                event["kind"] = json!("message");
                if origin == "human" {
                    event["speaker"] = json!(self.inner.data().binding.user_id);
                }
                if let Some(text) = item["content"].as_str() {
                    return self.text(event, text);
                }
                if let Some(parts) = item["content"].as_array() {
                    for (index, part) in parts.iter().enumerate() {
                        let mut event = event.clone();
                        event["metadata"]["part_index"] = json!(index);
                        if let Some(text) =
                            part["text"].as_str().or_else(|| part["refusal"].as_str())
                        {
                            self.text(event, text)?;
                        } else {
                            event["kind"] = json!("attachment");
                            if let Some(url) = part["image_url"].as_str() {
                                if let Some((prefix, data)) = url
                                    .split_once(";base64,")
                                    .filter(|(p, _)| p.starts_with("data:"))
                                {
                                    event["media_type"] = json!(&prefix[5..]);
                                    self.binary(event, data)?;
                                } else {
                                    self.text(event, url)?;
                                }
                            } else {
                                self.text(event, &part.to_string())?;
                            }
                        }
                    }
                    Ok(())
                } else {
                    self.text(event, &item.to_string())
                }
            }
            "input_audio" => {
                event["kind"] = json!("attachment");
                event["media_type"] = json!(format!(
                    "audio/{}",
                    item["input_audio"]["format"].as_str().unwrap_or("unknown")
                ));
                self.binary(
                    event,
                    item["input_audio"]["data"]
                        .as_str()
                        .context("音声の原文がありません")?,
                )
            }
            _ => self.text(event, &item.to_string()),
        }
    }

    fn notice(&mut self, text: &str, status: &str) -> Result<()> {
        self.text(
            json!({"kind":"other","origin":"runtime","status":status}),
            text,
        )
    }

    fn clear_active(&self) -> Result<()> {
        match std::fs::remove_file(self.dir.join("active.json")) {
            Ok(()) => sync_dir(&self.dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn pending_transition(&mut self, preview: &SessionData) -> Result<()> {
        let before = self.inner.data().history.len();
        self.items(&json!(&preview.history[before..]), "runtime", None)
    }
}

impl ConversationStore for RecordedConversation<'_> {
    fn data(&self) -> &SessionData {
        self.inner.data()
    }
    fn replays_history(&self) -> bool {
        self.inner.replays_history()
    }
    fn set_history_parent(&mut self, conversation: Option<&str>, call_id: Option<&str>) {
        if conversation.is_some() {
            self.parent = json!({"conversation":conversation,"call_id":call_id});
        }
    }
    fn record_control_input(&mut self, text: &str) -> Result<()> {
        self.text(
            json!({"kind":"other","origin":"human","metadata":{"operation":"command"}}),
            text,
        )
    }
    fn begin_turn(&mut self, input: &Value) -> Result<()> {
        self.begin_turn_with_source(input, input, &json!([]), "unknown")
    }
    fn begin_turn_with_source(
        &mut self,
        input: &Value,
        original: &Value,
        runtime: &Value,
        origin: &str,
    ) -> Result<()> {
        if self.inner.data().status == SessionStatus::Running {
            bail!("会話はすでに実行中です");
        }
        self.turn = Uuid::new_v4().to_string();
        atomic_write(
            &self.dir.join("active.json"),
            &serde_json::to_vec(&json!({"turn":self.turn}))?,
            None,
        )?;
        sync_dir(&self.dir)?;
        self.items(original, origin, None)?;
        self.items(runtime, "runtime", None)?;
        self.inner.begin_turn(input)
    }
    fn record_response(&mut self, id: &str, output: &[Value]) -> Result<()> {
        self.items(&json!(output), "model", Some(id))?;
        self.inner.record_response(id, output)
    }
    fn checkpoint_tool_result(&mut self, result: &Value, plan: &TaskPlan) -> Result<()> {
        self.checkpoint_tool_result_with_raw(result, result, plan)
    }
    fn checkpoint_tool_result_with_raw(
        &mut self,
        result: &Value,
        raw: &Value,
        plan: &TaskPlan,
    ) -> Result<()> {
        self.item(raw, "tool", None)?;
        self.inner.checkpoint_tool_result(result, plan)
    }
    fn replace_plan(&mut self, plan: &TaskPlan) -> Result<()> {
        self.text(
            json!({"kind":"other","origin":"runtime","metadata":{"operation":"replace_plan"}}),
            &serde_json::to_string(plan)?,
        )?;
        self.inner.replace_plan(plan)
    }
    fn record_runtime_input(&mut self, input: &Value) -> Result<()> {
        self.items(input, "runtime", None)?;
        self.inner.record_runtime_input(input)
    }
    fn record_usage(&mut self, delta: &UsageSummary) -> Result<()> {
        self.inner.record_usage(delta)
    }
    fn replace_history(
        &mut self,
        history: Vec<Value>,
        record: CompactionRecord,
    ) -> Result<CompactionRecord> {
        self.inner.data().ensure_compactable()?;
        let through = self.next.saturating_sub(1);
        self.text(json!({"kind":"summary","origin":"model","metadata":{"source_sequence_start":1,"source_sequence_end":through,"compaction":record}}), &serde_json::to_string(&without_reasoning(&history))?)?;
        self.inner.replace_history(history, record)
    }
    fn skip_pending(&mut self, message: &str) -> Result<()> {
        let mut preview = self.inner.data().clone();
        preview.skip_pending(message);
        self.pending_transition(&preview)?;
        self.inner.skip_pending(message)
    }
    fn complete(&mut self) -> Result<()> {
        self.notice("ターンが完了しました", "ok")?;
        self.inner.complete()?;
        self.clear_active()
    }
    fn fail(&mut self, error: &str) -> Result<()> {
        let mut preview = self.inner.data().clone();
        preview.fail(error)?;
        self.pending_transition(&preview)?;
        self.inner.fail(error)?;
        self.clear_active()
    }
    fn switch_model(&mut self, choice: &ModelChoice, endpoint: &str) -> Result<()> {
        self.inner.switch_model(choice, endpoint)?;
        self.notice(
            &format!(
                "モデルを {}（接続先 {}）に切り替えました",
                choice.model, choice.provider
            ),
            "ok",
        )
    }
}

fn without_endpoint(mut manifest: Value) -> Value {
    if let Some(binding) = manifest["binding"].as_object_mut() {
        binding.remove("endpoint");
    }
    manifest
}

impl Drop for RecordedConversation<'_> {
    fn drop(&mut self) {
        // Ctrl+C や Future の破棄でも、後から実行結果を成功と誤認しない。
        if self.inner.data().status == SessionStatus::Running {
            if let Err(error) = self.fail("実行が中断されました。ツールの副作用は不明です。")
            {
                eprintln!("中断履歴の保存に失敗しました: {error:#}");
            }
        }
    }
}

/// モデルの思考過程はやり取りの原文ではないため、履歴に残さない。
fn is_reasoning(item: &Value) -> bool {
    item["type"] == "reasoning"
}

/// 圧縮後の履歴から、思考過程と、その暗号化された内容を除く。
fn without_reasoning(history: &[Value]) -> Vec<Value> {
    history
        .iter()
        .filter(|item| !is_reasoning(item))
        .map(|item| {
            let mut item = item.clone();
            if let Some(fields) = item.as_object_mut() {
                fields.remove("encrypted_content");
            }
            item
        })
        .collect()
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}
