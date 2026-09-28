//! ChatGPT のデバイス認証と、ano 専用の認証情報保存。

use super::super::fs::atomic_write;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions, TryLockError},
    io::ErrorKind,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const ISSUER: &str = "https://auth.openai.com";
// Codex の公開 OAuth クライアント識別子。秘密情報ではない。
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(900);
const REFRESH_MARGIN: u64 = 60;

#[derive(Clone)]
pub struct ChatGptAuth {
    path: PathBuf,
    http: Client,
    issuer: String,
}

// Debug を実装しない。トークンをログやエラーへ出力しない。
#[derive(Serialize, Deserialize)]
pub(super) struct Credentials {
    pub access_token: String,
    refresh_token: String,
    pub account_id: String,
    expires_at: u64,
}

#[derive(Debug, Serialize)]
pub struct AuthStatus {
    pub logged_in: bool,
    pub needs_refresh: bool,
}

impl ChatGptAuth {
    #[cfg(test)]
    pub(super) fn for_test(path: &Path, issuer: &str) -> Self {
        let mut auth = Self::new(Some(path)).unwrap();
        auth.issuer = issuer.to_string();
        auth
    }

    pub fn new(path: Option<&Path>) -> Result<Self> {
        let path = match path {
            Some(path) => path.to_path_buf(),
            None => std::env::home_dir()
                .context(
                    "ホームディレクトリを取得できません。api.chatgpt_auth_file を指定してください",
                )?
                .join(".ano/auth/chatgpt.json"),
        };
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        Ok(Self {
            path,
            http: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()?,
            issuer: ISSUER.into(),
        })
    }

    pub fn status(&self) -> Result<AuthStatus> {
        let credentials = self.load()?;
        Ok(AuthStatus {
            logged_in: credentials.is_some(),
            needs_refresh: credentials.is_some_and(|c| c.expires_at <= now() + REFRESH_MARGIN),
        })
    }

    pub async fn logout(&self) -> Result<bool> {
        let _lock = self.lock().await?;
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).context("ChatGPT の認証情報を削除できません"),
        }
    }

    /// 表示用 URL と確認コードだけを呼び出し元へ渡す。
    pub async fn login(&self, show_code: impl FnOnce(&str, &str)) -> Result<()> {
        tokio::time::timeout(LOGIN_TIMEOUT, self.device_login(show_code))
            .await
            .context("ChatGPT ログインが15分以内に完了しませんでした。ano auth login を再実行してください")?
    }

    async fn device_login(&self, show_code: impl FnOnce(&str, &str)) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/api/accounts/deviceauth/usercode", self.issuer))
            .json(&json!({"client_id": CLIENT_ID}))
            .send()
            .await
            .context("ChatGPT ログインを開始できません")?;
        if response.status() == StatusCode::NOT_FOUND {
            bail!("デバイス認証を利用できません。ChatGPT のセキュリティ設定またはワークスペース設定を確認してください");
        }
        let code = auth_json(response).await?;
        let user_code = code
            .get("user_code")
            .or_else(|| code.get("usercode"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .context("ChatGPT から確認コードを取得できませんでした")?;
        let device_id = required_string(&code, "device_auth_id")?;
        let mut interval = code
            .get("interval")
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str()?.trim().parse().ok())
            })
            .unwrap_or(5)
            .max(1);
        show_code(&format!("{}/codex/device", self.issuer), user_code);
        loop {
            let response = self
                .http
                .post(format!("{}/api/accounts/deviceauth/token", self.issuer))
                .json(&json!({"device_auth_id": device_id, "user_code": user_code}))
                .send()
                .await
                .context("ChatGPT のログイン状況を取得できません")?;
            let status = response.status();
            if status.is_success() {
                let grant = auth_json(response).await?;
                let tokens = self
                    .token_request(&[
                        ("grant_type", "authorization_code"),
                        ("code", required_string(&grant, "authorization_code")?),
                        ("code_verifier", required_string(&grant, "code_verifier")?),
                        (
                            "redirect_uri",
                            &format!("{}/deviceauth/callback", self.issuer),
                        ),
                        ("client_id", CLIENT_ID),
                    ])
                    .await?;
                let credentials = credentials_from_tokens(&tokens, None)?;
                let _lock = self.lock().await?;
                return self.save(&credentials);
            }
            let body: Value = response.json().await.unwrap_or(Value::Null);
            let error = body["error"]
                .as_str()
                .or_else(|| body["error"]["code"].as_str());
            match error {
                Some("access_denied" | "expired_token") => {
                    bail!("ChatGPT ログインが拒否されたか、確認コードが失効しました")
                }
                Some("slow_down") => interval = interval.saturating_add(5),
                Some("authorization_pending") => {}
                _ if matches!(status.as_u16(), 403 | 404) => {}
                _ if status == StatusCode::TOO_MANY_REQUESTS => {
                    interval = interval.saturating_add(5)
                }
                _ => bail!("ChatGPT ログインの確認に失敗しました ({status})"),
            }
            tokio::time::sleep(Duration::from_secs(interval.min(LOGIN_TIMEOUT.as_secs()))).await;
        }
    }

    /// 複数プロセスでも refresh token の更新が競合しないよう、再読込から保存までロックする。
    /// rejected は 401 を受けた access token。同時更新済みなら新しいトークンをそのまま返す。
    pub(super) async fn credentials(&self, rejected: Option<&str>) -> Result<Credentials> {
        let _lock = self.lock().await?;
        let current = self
            .load()?
            .context("ChatGPT にログインしていません。ano auth login を実行してください")?;
        let needs_refresh = current.expires_at <= now() + REFRESH_MARGIN
            || rejected == Some(current.access_token.as_str());
        if !needs_refresh {
            return Ok(current);
        }
        let tokens = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &current.refresh_token),
                ("client_id", CLIENT_ID),
            ])
            .await?;
        let updated = credentials_from_tokens(&tokens, Some(&current))?;
        if updated.account_id != current.account_id {
            bail!("更新後の ChatGPT アカウントが一致しません。ano auth login を再実行してください");
        }
        self.save(&updated)?;
        Ok(updated)
    }

    async fn token_request(&self, fields: &[(&str, &str)]) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}/oauth/token", self.issuer))
            .form(fields)
            .send()
            .await
            .context("ChatGPT の認証サーバーに接続できません")?;
        auth_json(response).await
    }

    fn load(&self) -> Result<Option<Credentials>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("ChatGPT の認証情報を読み込めません"),
        };
        let credentials: Credentials = serde_json::from_slice(&bytes).map_err(|_| {
            anyhow::anyhow!(
                "ChatGPT の認証情報が破損しています。ano auth login を再実行してください"
            )
        })?;
        if credentials.access_token.is_empty()
            || credentials.refresh_token.is_empty()
            || credentials.account_id.is_empty()
        {
            bail!("ChatGPT の認証情報が不完全です。ano auth login を再実行してください");
        }
        Ok(Some(credentials))
    }

    fn save(&self, credentials: &Credentials) -> Result<()> {
        #[cfg(unix)]
        let permissions = {
            use std::os::unix::fs::PermissionsExt;
            Some(std::fs::Permissions::from_mode(0o600))
        };
        #[cfg(not(unix))]
        let permissions = None;
        atomic_write(&self.path, &serde_json::to_vec(credentials)?, permissions)
            .context("ChatGPT の認証情報を保存できません")
    }

    async fn lock(&self) -> Result<File> {
        let parent = self.path.parent().context("認証情報の保存先が不正です")?;
        let mut directories = std::fs::DirBuilder::new();
        directories.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directories.mode(0o700);
        }
        directories
            .create(parent)
            .context("認証情報の保存先を作成できません")?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut lock_name = self.path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let file = options
            .open(PathBuf::from(lock_name))
            .context("認証情報をロックできません")?;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match file.try_lock() {
                    Ok(()) => return Ok(file),
                    Err(TryLockError::WouldBlock) => {
                        tokio::time::sleep(Duration::from_millis(50)).await
                    }
                    Err(TryLockError::Error(error)) => {
                        return Err(error).context("認証情報をロックできません")
                    }
                }
            }
        })
        .await
        .context("別のプロセスが ChatGPT 認証情報を更新中です。しばらくして再実行してください")?
    }
}

async fn auth_json(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    // エラー本文にトークンや認可コードが含まれる場合もあるため表示しない。
    if !status.is_success() {
        bail!("ChatGPT 認証に失敗しました ({status})。ano auth login で再ログインしてください");
    }
    response
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("ChatGPT 認証サーバーの応答が不正です"))
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("ChatGPT 認証サーバーの応答に {field} がありません"))
}

fn credentials_from_tokens(tokens: &Value, previous: Option<&Credentials>) -> Result<Credentials> {
    let access_token = required_string(tokens, "access_token")?.to_string();
    let refresh_token = tokens["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| previous.map(|c| c.refresh_token.as_str()))
        .context("ChatGPT の更新用トークンがありません")?
        .to_string();
    // TLS で取得したトークンのメタデータを送信先のルーティングにのみ使用する。
    // 署名検証やローカルの権限付与には使わず、認証は OpenAI 側で行う。
    let claims = token_claims(&access_token);
    let id_claims = tokens["id_token"]
        .as_str()
        .map(token_claims)
        .unwrap_or(Value::Null);
    let account_id = claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .or_else(|| id_claims["https://api.openai.com/auth"]["chatgpt_account_id"].as_str())
        .or_else(|| previous.map(|c| c.account_id.as_str()))
        .filter(|s| !s.is_empty())
        .context("ChatGPT アカウント識別子を取得できません")?
        .to_string();
    let expires_at = tokens["expires_in"]
        .as_u64()
        .map(|seconds| now().saturating_add(seconds))
        .or_else(|| claims["exp"].as_u64())
        .unwrap_or_else(|| now() + 3600);
    Ok(Credentials {
        access_token,
        refresh_token,
        account_id,
        expires_at,
    })
}

fn token_claims(token: &str) -> Value {
    token
        .split('.')
        .nth(1)
        .and_then(|part| URL_SAFE_NO_PAD.decode(part).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests;
