//! OAuth for directly connected Streamable HTTP MCP servers.
//!
//! `ano mcp login` runs the authorization code flow with PKCE (registering
//! ano as a client dynamically when the server supports it) and saves the
//! tokens in an [`OAuthStore`]. Connections then send the access token and
//! refresh it when it expires or the server rejects it.

use crate::{domain::mcp::McpServerConfig, infrastructure::fs::atomic_write};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use axum::{extract::RawQuery, response::Html, routing::get, Router};
use rmcp::transport::{
    AuthClient, AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession,
    CredentialRefreshGuard, CredentialStore, StoredCredentials,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    future::IntoFuture,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{mpsc, Mutex},
};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const CLIENT_NAME: &str = "ano";
const CALLBACK_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>ano</title><p>Authorization finished. You can close this window and return to ano.</p>";

/// Saved OAuth credentials of MCP servers: one JSON file per server label and
/// URL, readable only by the current user.
#[derive(Clone)]
pub struct OAuthStore {
    directory: Option<PathBuf>,
    /// Serializes token refreshes of one credential file within this process.
    refresh_locks: Arc<StdMutex<HashMap<PathBuf, Arc<Mutex<()>>>>>,
}

impl OAuthStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self::with_directory(Some(directory.into()))
    }

    /// `~/.ano/oauth`. Without a known home directory, OAuth servers cannot
    /// be used, but other servers are unaffected.
    pub fn default_location() -> Self {
        Self::with_directory(std::env::home_dir().map(|home| home.join(".ano").join("oauth")))
    }

    fn with_directory(directory: Option<PathBuf>) -> Self {
        Self {
            directory,
            refresh_locks: Default::default(),
        }
    }

    /// Credentials are keyed by the URL too, so pointing a label at another
    /// server never sends it the previous server's tokens.
    fn path(&self, server: &McpServerConfig) -> Result<PathBuf> {
        let directory = self
            .directory
            .as_ref()
            .context("cannot locate saved MCP OAuth credentials: the home directory is unknown")?;
        let url = server.url.as_deref().unwrap_or_default();
        let digest = hex::encode(&Sha256::digest(url.as_bytes())[..8]);
        Ok(directory.join(format!("{}-{digest}.json", server.label)))
    }

    fn credential_store(&self, server: &McpServerConfig) -> Result<FileCredentialStore> {
        let path = self.path(server)?;
        let refresh_lock = Arc::clone(
            self.refresh_locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(path.clone())
                .or_default(),
        );
        Ok(FileCredentialStore { path, refresh_lock })
    }

    /// Delete the saved credentials of `server`. Returns whether any existed.
    pub fn remove(&self, server: &McpServerConfig) -> Result<bool> {
        let path = self.path(server)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("failed to delete {}", path.display()))
            }
        }
    }

    /// An HTTP client that authorizes requests to `server` with the saved
    /// credentials, refreshing the access token when needed.
    pub(crate) async fn authorized_client(
        &self,
        server: &McpServerConfig,
    ) -> Result<AuthClient<reqwest::Client>> {
        let label = &server.label;
        let mut manager = AuthorizationManager::new(server.url.as_deref().unwrap_or_default())
            .await
            .with_context(|| format!("invalid URL of MCP server '{label}'"))?;
        manager.set_credential_store(self.credential_store(server)?);
        let authorized = manager
            .initialize_from_store()
            .await
            .with_context(|| format!("failed to load OAuth credentials of MCP server '{label}'"))?;
        if !authorized {
            bail!("MCP server '{label}' requires OAuth authorization; run `ano mcp login {label}`");
        }
        Ok(AuthClient::new(reqwest::Client::new(), manager))
    }

    /// Run the authorization code flow for `server` and save its credentials.
    /// `show_url` receives the URL the user must open to authorize ano; the
    /// browser is then redirected to a temporary listener on 127.0.0.1.
    pub async fn login(&self, server: &McpServerConfig, show_url: impl FnOnce(&str)) -> Result<()> {
        let label = &server.label;
        if !server.oauth {
            bail!("MCP server '{label}' does not use OAuth; set `oauth = true` in its config");
        }
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to listen for the OAuth callback")?;
        let redirect_uri = format!(
            "http://127.0.0.1:{}/callback",
            listener.local_addr()?.port()
        );

        let mut manager = AuthorizationManager::new(server.url.as_deref().unwrap_or_default())
            .await
            .with_context(|| format!("invalid URL of MCP server '{label}'"))?;
        manager.set_credential_store(self.credential_store(server)?);
        let resolution = manager.resolve_metadata().await.with_context(|| {
            format!("failed to discover the OAuth authorization server of MCP server '{label}'")
        })?;
        manager.set_metadata(resolution.metadata);
        let mut request = AuthorizationRequest::new(&redirect_uri).with_client_name(CLIENT_NAME);
        if let Some(scopes) = &server.oauth_scopes {
            request = request.with_scopes(scopes);
        }
        let session = AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, error)| error)
            .with_context(|| {
                format!("failed to start OAuth authorization of MCP server '{label}'")
            })?;

        show_url(session.get_authorization_url());
        let query = tokio::time::timeout(LOGIN_TIMEOUT, receive_callback(listener))
            .await
            .with_context(|| {
                format!(
                    "OAuth authorization was not completed within {} seconds",
                    LOGIN_TIMEOUT.as_secs()
                )
            })??;
        // The exchange validates the state parameter and saves the tokens.
        session
            .handle_callback_url(&format!("{redirect_uri}?{query}"))
            .await
            .with_context(|| format!("OAuth authorization of MCP server '{label}' failed"))?;
        Ok(())
    }
}

/// Wait for the authorization server to redirect the browser back and return
/// the callback's query string.
async fn receive_callback(listener: TcpListener) -> Result<String> {
    let (sender, mut receiver) = mpsc::channel::<String>(1);
    let app = Router::new().route(
        "/callback",
        get(move |RawQuery(query): RawQuery| {
            let sender = sender.clone();
            async move {
                sender.try_send(query.unwrap_or_default()).ok();
                Html(CALLBACK_PAGE)
            }
        }),
    );
    // Connections are served on their own tasks, so the page is still sent
    // after this future stops accepting new connections.
    let query = tokio::select! {
        served = axum::serve(listener, app).into_future() => {
            served.context("OAuth callback listener failed")?;
            bail!("OAuth callback listener stopped unexpectedly");
        }
        query = receiver.recv() => query.context("OAuth callback listener stopped unexpectedly")?,
    };
    let parameters: HashMap<String, String> =
        reqwest::Url::parse(&format!("http://127.0.0.1/callback?{query}"))
            .context("invalid OAuth callback")?
            .query_pairs()
            .into_owned()
            .collect();
    if let Some(error) = parameters.get("error") {
        match parameters.get("error_description") {
            Some(description) => bail!("OAuth authorization was denied: {error}: {description}"),
            None => bail!("OAuth authorization was denied: {error}"),
        }
    }
    Ok(query)
}

struct FileCredentialStore {
    path: PathBuf,
    refresh_lock: Arc<Mutex<()>>,
}

fn store_error(path: &Path, error: impl std::fmt::Display) -> AuthError {
    AuthError::CredentialStoreError(format!("{}: {error}", path.display()))
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| store_error(&self.path, error)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(store_error(&self.path, error)),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec_pretty(&credentials)
            .map_err(|error| store_error(&self.path, error))?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || write_private(&path, &bytes))
            .await
            .map_err(|error| store_error(&self.path, error))?
            .map_err(|error| store_error(&self.path, format!("{error:#}")))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        match tokio::fs::remove_file(&self.path).await {
            Err(error) if error.kind() != ErrorKind::NotFound => {
                Err(store_error(&self.path, error))
            }
            _ => Ok(()),
        }
    }

    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        let guard = Arc::clone(&self.refresh_lock).lock_owned().await;
        Ok(Some(CredentialRefreshGuard::new(guard)))
    }
}

/// Save `bytes` so that only the current user can read them.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().context("file has no parent directory")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
        atomic_write(path, bytes, Some(std::fs::Permissions::from_mode(0o600)))
    }
    #[cfg(not(unix))]
    atomic_write(path, bytes, None)
}

#[cfg(test)]
mod tests {
    use super::{receive_callback, OAuthStore};
    use crate::domain::mcp::{McpApprovalMode, McpServerConfig, McpTransport};
    use rmcp::transport::{CredentialStore, StoredCredentials};
    use tokio::net::TcpListener;

    fn server(label: &str, url: &str) -> McpServerConfig {
        McpServerConfig {
            label: label.into(),
            transport: McpTransport::StreamableHttp,
            url: Some(url.into()),
            tunnel_id: None,
            command: None,
            args: vec![],
            cwd: None,
            env_vars: Default::default(),
            description: None,
            authorization_env: None,
            oauth: true,
            oauth_scopes: None,
            allowed_tools: None,
            disabled_tools: vec![],
            tool_catalog: None,
            require_approval: McpApprovalMode::Always,
            reuse_connection: true,
        }
    }

    #[tokio::test]
    async fn saves_credentials_privately_per_label_and_url() {
        let directory = tempfile::tempdir().unwrap();
        let store = OAuthStore::new(directory.path().join("oauth"));
        let first = server("docs", "https://a.test/mcp");
        let credentials = store.credential_store(&first).unwrap();
        assert!(credentials.load().await.unwrap().is_none());

        credentials
            .save(StoredCredentials::new("client".into(), None, vec![], None))
            .await
            .unwrap();
        let loaded = credentials.load().await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "client");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&credentials.path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let moved = server("docs", "https://b.test/mcp");
        assert!(store
            .credential_store(&moved)
            .unwrap()
            .load()
            .await
            .unwrap()
            .is_none());

        assert!(store.remove(&first).unwrap());
        assert!(!store.remove(&first).unwrap());
    }

    #[tokio::test]
    async fn authorized_client_requires_login() {
        let directory = tempfile::tempdir().unwrap();
        let store = OAuthStore::new(directory.path());
        let error = store
            .authorized_client(&server("docs", "http://127.0.0.1:9/mcp"))
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("ano mcp login docs"));
    }

    async fn callback(query: &str) -> anyhow::Result<String> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/callback?{query}", listener.local_addr().unwrap());
        let waiting = tokio::spawn(receive_callback(listener));
        let page = reqwest::get(url).await.unwrap().text().await.unwrap();
        assert!(page.contains("Authorization finished"));
        waiting.await.unwrap()
    }

    #[tokio::test]
    async fn callback_returns_the_query_or_the_denial() {
        assert_eq!(
            callback("code=abc&state=xyz").await.unwrap(),
            "code=abc&state=xyz"
        );
        let error = callback("error=access_denied&error_description=User+cancelled")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("access_denied: User cancelled"));
    }
}
