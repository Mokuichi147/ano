//! chronotope の原文履歴。ローカルへの保存を先に確定し、HTTP で再送可能な同期を行う。

mod journal;
#[cfg(test)]
mod tests;
mod tools;

use crate::{
    application::{
        ports::{ConversationStore, HistoryBackend},
        registry::ToolRegistry,
    },
    domain::session::SessionBinding,
    infrastructure::{fs::atomic_write, memory_store::MemoryConversation},
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::{
    header::{HeaderMap, HeaderValue, AUTHORIZATION},
    Client, Url,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistorySettings {
    pub enabled: bool,
    pub base_url: String,
    pub data_dir: PathBuf,
    pub principal: String,
    pub token_env: Option<String>,
    pub timeout_secs: u64,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "http://127.0.0.1:7878".into(),
            data_dir: ".ano/history".into(),
            principal: "ano".into(),
            token_env: None,
            timeout_secs: 5,
        }
    }
}

impl HistorySettings {
    pub fn validate(&self) -> Result<()> {
        let url = Url::parse(&self.base_url).context("history.base_url が不正です")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("history.base_url には認証情報・クエリを含まない HTTP(S) URL を指定してください");
        }
        if self.principal.trim().is_empty() || self.principal == "anonymous" {
            bail!("history.principal に匿名以外の主体を指定してください");
        }
        HeaderValue::from_str(&self.principal)
            .context("history.principal がヘッダーに使用できません")?;
        if self.timeout_secs == 0 || self.data_dir.as_os_str().is_empty() {
            bail!("history.timeout_secs は正の数、data_dir は空でないパスにしてください");
        }
        Ok(())
    }
}

pub struct Chronotope {
    settings: HistorySettings,
    client: Client,
}

impl Chronotope {
    pub fn new(settings: HistorySettings) -> Result<Self> {
        settings.validate()?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-chronotope-principal",
            HeaderValue::from_str(&settings.principal)?,
        );
        headers.insert("x-chronotope-kind", HeaderValue::from_static("agent"));
        if let Some(name) = &settings.token_env {
            let token = std::env::var(name)
                .with_context(|| format!("履歴用の認証環境変数 {name} がありません"))?;
            if token.is_empty() {
                bail!("履歴用の認証環境変数 {name} が空です");
            }
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        let client = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(settings.timeout_secs))
            // 実行終了時の同期で、到達できない接続先を timeout_secs まで待たない。
            .connect_timeout(Duration::from_secs(settings.timeout_secs.min(2)))
            .build()?;
        Ok(Self { settings, client })
    }

    pub fn from_settings(
        settings: &HistorySettings,
        registry: &ToolRegistry,
    ) -> Result<Option<Arc<Self>>> {
        if !settings.enabled {
            return Ok(None);
        }
        let history = Arc::new(Self::new(settings.clone())?);
        history.register_tools(registry)?;
        Ok(Some(history))
    }

    pub fn register_tools(self: &Arc<Self>, registry: &ToolRegistry) -> Result<()> {
        tools::register(self, registry)
    }

    fn owner_dir(&self, user: &str) -> Result<PathBuf> {
        if user.is_empty() || user == "anonymous" {
            bail!("履歴の所有者が不正です");
        }
        HeaderValue::from_str(user).context("履歴の所有者がヘッダーに使用できません")?;
        // 接続先や主体を変更しても、別の保存先へ履歴を誤送信しない。
        let key = serde_json::to_vec(&(
            self.settings.base_url.trim_end_matches('/'),
            &self.settings.principal,
            user,
        ))?;
        Ok(self
            .settings
            .data_dir
            .join(hex::encode(Sha256::digest(key))))
    }

    async fn post(&self, user: &str, path: &str, body: &Value) -> Result<Value> {
        let response = self
            .client
            .post(format!(
                "{}/v1/{path}",
                self.settings.base_url.trim_end_matches('/')
            ))
            .header("x-chronotope-on-behalf-of", HeaderValue::from_str(user)?)
            .json(body)
            .send()
            .await
            .context("chronotope に接続できません")?;
        let status = response.status();
        // エラー本文に履歴や認証情報が含まれていてもログへ流さない。
        if status.is_client_error() && !matches!(status.as_u16(), 408 | 429) {
            return Err(Rejected(status.as_u16()).into());
        }
        if !status.is_success() {
            bail!("chronotope が HTTP {} を返しました", status.as_u16());
        }
        let value: Value = response
            .json()
            .await
            .context("chronotope の JSON 応答が不正です")?;
        if value.get("error").is_some_and(|v| !v.is_null()) {
            bail!("chronotope が操作エラーを返しました");
        }
        Ok(value)
    }

    pub fn status(&self, user: &str) -> Result<Value> {
        let root = self.owner_dir(user)?;
        let pending = pending_events(&root)?;
        Ok(json!({"pending_events":pending.len(), "synchronized":pending.is_empty()}))
    }

    /// 読み取り操作は固定し、所有者と委任元をモデルに指定させない。
    pub async fn query(&self, user: &str, op: &str, arguments: Value) -> Result<Value> {
        let mut args = tools::validate_query(op, arguments)?;
        // 未送信分があるときだけ先に送る。送れなかった分は local_sync で示す。
        let sync_error = if pending_events(&self.owner_dir(user)?)?.is_empty() {
            None
        } else {
            self.sync(user).await.err()
        };
        args.insert("op".into(), json!(op));
        args.insert("budget_ms".into(), json!(1000));
        let mut result = self.post(user, "query", &Value::Object(args)).await?;
        result["local_sync"] = self.status(user)?;
        if result["local_sync"]["synchronized"] != true {
            result["local_sync"]["warning"] =
                json!("未送信の履歴があります。検索結果に存在しない発言があり得ます。");
        }
        if let Some(error) = sync_error {
            result["local_sync"]["error"] = json!(format!("{error:#}"));
        }
        Ok(result)
    }

    /// 1 会話分の未送信イベントを記録順に送り、受領を確認した位置まで送信済みにする。
    async fn sync_conversation(&self, user: &str, dir: &Path, sent: &mut usize) -> Result<()> {
        let pending = pending_in(dir)?;
        let mut done = 0;
        let mut limit = MAX_BATCH_EVENTS;
        while done < pending.len() {
            let mut batch = Vec::new();
            let mut bytes = 0;
            for path in pending[done..].iter().take(limit) {
                let raw = std::fs::read(path)?;
                if !batch.is_empty() && bytes + raw.len() > MAX_BATCH_BYTES {
                    break;
                }
                bytes += raw.len();
                batch.push(serde_json::from_slice::<Value>(&raw).with_context(|| {
                    format!("ローカルの原文履歴が破損しています: {}", path.display())
                })?);
            }
            match self.record(user, &batch).await {
                Ok(()) => {}
                // 1 件ずつ送り直し、受け付けられる分を進めて拒否されたイベントを特定する。
                Err(error) if error.is::<Rejected>() && batch.len() > 1 => {
                    limit = 1;
                    continue;
                }
                Err(error) if error.is::<Rejected>() => {
                    return Err(error.context(format!(
                        "記録順 {} のイベントが受け付けられませんでした。以降は送信を止めています: {}",
                        batch[0]["sequence"],
                        pending[done].display()
                    )));
                }
                Err(error) => return Err(error),
            }
            done += batch.len();
            *sent += batch.len();
            let through = batch
                .last()
                .and_then(|event| event["sequence"].as_u64())
                .context("ローカルの原文履歴に記録順がありません")?;
            atomic_write(&dir.join(SENT_FILE), through.to_string().as_bytes(), None)?;
            sync_dir(dir)?;
        }
        Ok(())
    }

    async fn record(&self, user: &str, batch: &[Value]) -> Result<()> {
        let reply = self
            .post(
                user,
                "write",
                &json!({"op":"record_events", "events":batch}),
            )
            .await?;
        // 成功の HTTP ステータスだけでは送信済みにしない。全 ID と順序の受領を確認する。
        let acks = reply["events"]
            .as_array()
            .context("chronotope の保存応答に events がありません")?;
        if reply["owner"] != user
            || acks.len() != batch.len()
            || acks.iter().zip(batch).any(|(ack, event)| {
                ack["event_id"] != event["event_id"]
                    || ack["sequence"] != event["sequence"]
                    || !ack["acquisition"].is_string()
            })
        {
            bail!("chronotope の保存応答が送信内容と一致しません。送信済み位置は更新しません");
        }
        Ok(())
    }
}

const MAX_BATCH_EVENTS: usize = 100;
const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;
/// 会話ごとに、chronotope が受領を確認した最後の記録順を保存する。
const SENT_FILE: &str = "sent";

/// chronotope が内容を理由に拒否した。再送しても成功しない。
#[derive(Debug)]
struct Rejected(u16);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "chronotope が HTTP {} で拒否しました", self.0)
    }
}

impl std::error::Error for Rejected {}

#[async_trait]
impl HistoryBackend for Chronotope {
    fn transcript_store(&self, binding: SessionBinding) -> Box<dyn ConversationStore> {
        Box::new(MemoryConversation::transcript_only(binding))
    }

    fn wrap<'a>(
        &self,
        store: &'a mut dyn ConversationStore,
    ) -> Result<Box<dyn ConversationStore + 'a>> {
        let root = self.owner_dir(&store.data().binding.user_id)?;
        Ok(Box::new(journal::RecordedConversation::open(root, store)?))
    }

    async fn sync(&self, user: &str) -> Result<Value> {
        let root = self.owner_dir(user)?;
        private_dir(&root)?;
        let lock = lock_file(&root.join("sync.lock"))?;
        if lock.try_lock().is_err() {
            let mut status = self.status(user)?;
            status["busy"] = json!(true);
            return Ok(status);
        }
        let mut sent = 0;
        let mut failures = Vec::new();
        // 会話は互いに独立しているため、1 会話の失敗で他の会話の送信を止めない。
        for dir in conversation_dirs(&root)? {
            if let Err(error) = self.sync_conversation(user, &dir, &mut sent).await {
                let rejected = error.is::<Rejected>();
                let name = dir.file_name().unwrap_or_default().to_string_lossy();
                failures.push(format!("会話 {name}: {error:#}"));
                // 接続できない場合は他の会話も失敗するため、待ち時間を重ねない。
                if !rejected {
                    break;
                }
            }
        }
        if !failures.is_empty() {
            bail!(
                "{sent} 件を送信しましたが、未送信の履歴が残っています。{}",
                failures.join("; ")
            );
        }
        let mut status = self.status(user)?;
        status["sent_events"] = json!(sent);
        Ok(status)
    }
}

fn private_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!("履歴ディレクトリはシンボリックリンクにできません");
            }
            if !metadata.is_dir() {
                bail!(
                    "履歴パスはディレクトリである必要があります: {}",
                    path.display()
                );
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        private_dir(parent)?;
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "履歴パスはディレクトリである必要があります: {}",
                    path.display()
                );
            }
        }
        Err(e) => return Err(e.into()),
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        sync_dir(parent)?;
    }
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    Ok(())
}

fn lock_file(path: &Path) -> Result<File> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("履歴ロックはシンボリックリンクにできません");
    }
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    Ok(opts.open(path)?)
}

fn conversation_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn sent_through(dir: &Path) -> Result<u64> {
    match std::fs::read_to_string(dir.join(SENT_FILE)) {
        Ok(text) => text
            .trim()
            .parse()
            .with_context(|| format!("送信済み位置が破損しています: {}", dir.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn event_path(dir: &Path, sequence: u64) -> PathBuf {
    dir.join(format!("{sequence:020}.json"))
}

/// 会話内の記録順は連続するため、送信済み位置の次から順に調べる。
/// 走査量は未送信件数に比例し、保存済みの履歴全体を読み直さない。
fn pending_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut sequence = sent_through(dir)?;
    loop {
        sequence = sequence
            .checked_add(1)
            .context("履歴の記録順が上限に達しました")?;
        let path = event_path(dir, sequence);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => paths.push(path),
            Ok(_) => bail!("原文履歴が通常のファイルではありません: {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
            Err(error) => return Err(error.into()),
        }
    }
}

/// 次に記録する順番。送信済みのファイルを消しても、同じ記録順を再利用しない。
fn next_sequence(dir: &Path) -> Result<u64> {
    (sent_through(dir)? + pending_in(dir)?.len() as u64)
        .checked_add(1)
        .context("履歴の記録順が上限に達しました")
}

fn pending_events(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for dir in conversation_dirs(root)? {
        paths.extend(pending_in(&dir)?);
    }
    Ok(paths)
}
