//! Directly connected MCP servers (`stdio` and `streamable_http`).
//!
//! An [`McpPool`] owns the connections and shares them between runs, so a
//! stdio server process is started once rather than once per task. The
//! application applies each run's user policy to the borrowed connections.

use crate::{
    application::ports::{DirectMcpServer, DirectMcpTool, McpGateway, McpServerFailure},
    domain::{
        mcp::{McpServerConfig, McpToolCatalog, McpTransport},
        policy::UserPolicy,
        tool::DIRECT_MCP_PREFIX,
    },
    infrastructure::mcp_oauth::OAuthStore,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use rmcp::{
    model::{CallToolRequestParams, ClientConfig, ProtocolVersion},
    service::RunningService,
    transport::{
        child_process::TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    RoleClient, ServiceExt,
};
use serde_json::{json, Map, Value};
use std::{collections::HashSet, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    process::Command,
    sync::{watch, Mutex},
};

type McpClient = RunningService<RoleClient, ClientConfig>;

/// Longest connection error kept for progress output and events.
const MAX_FAILURE_CHARS: usize = 300;

/// A connection error made safe to show: URLs lose their credentials and
/// query (where API keys are often passed), and long text is cut, since the
/// error may carry text the server sent.
fn redact_error(error: &str) -> String {
    static URL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?P<scheme>[a-zA-Z][a-zA-Z0-9+.-]*://)(?:[^\s/@]*@)?(?P<rest>[^\s?#)]*)(?:[?#][^\s)]*)?").unwrap()
    });
    // Credentials a library may print in other forms, such as headers.
    static SECRET: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b((?:[a-z0-9]+[_-])*(?:bearer|basic|authorization|token|api[_-]?key|secret|password))(\s*[:=]\s*|\s+)(?:(?:bearer|basic)\s+)?[^\s,;)\]}]+").unwrap()
    });
    let redacted = URL.replace_all(error, "$scheme$rest");
    let redacted = SECRET.replace_all(&redacted, "$1$2[redacted]");
    let mut text: String = redacted.chars().take(MAX_FAILURE_CHARS).collect();
    if redacted.chars().count() > MAX_FAILURE_CHARS {
        text.push('…');
    }
    text
}

/// The client ano connects as, with the protocol version pinned to the
/// newest one that still has the `initialize` handshake. rmcp 3.5 made
/// `2026-07-28`, which has none, the default; a server that answers it with
/// an older version (such as GitHub's MCP server) then gets its connection
/// closed at the `initialized` notification.
fn client_config() -> ClientConfig {
    ClientConfig::default().with_protocol_version(ProtocolVersion::V_2025_11_25)
}

const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Shared connections to the directly connected MCP servers in a config.
///
/// Servers are connected lazily on first use. A connection whose transport
/// has closed (for example, a crashed stdio process) is replaced the next
/// time a run needs it. Servers with `reuse_connection = false` get a fresh
/// connection for every run instead.
pub struct McpPool {
    configs: Vec<McpServerConfig>,
    /// One slot per entry in `configs`.
    slots: Vec<Mutex<Option<Arc<ConnectedMcpServer>>>>,
    oauth: OAuthStore,
    shutdown_signal: watch::Sender<bool>,
}

impl McpPool {
    pub fn new(configs: Vec<McpServerConfig>) -> Self {
        let slots = configs.iter().map(|_| Mutex::new(None)).collect();
        let (shutdown_signal, _) = watch::channel(false);
        Self {
            configs,
            slots,
            oauth: OAuthStore::default_location(),
            shutdown_signal,
        }
    }

    /// Use `store` for the credentials of servers with `oauth = true`.
    pub fn with_oauth_store(mut self, store: OAuthStore) -> Self {
        self.oauth = store;
        self
    }

    pub fn configs(&self) -> &[McpServerConfig] {
        &self.configs
    }

    /// Labels of servers that currently hold a live shared connection.
    pub async fn connected_servers(&self) -> Vec<String> {
        let mut labels = Vec::new();
        for (config, slot) in self.configs.iter().zip(&self.slots) {
            if slot
                .lock()
                .await
                .as_ref()
                .is_some_and(|server| server.is_healthy())
            {
                labels.push(config.label.clone());
            }
        }
        labels
    }

    /// Close every shared connection and stop stdio server processes.
    /// Connections still used by a running task and pending connection attempts
    /// are cancelled. This pool cannot be used again after shutdown.
    pub async fn shutdown(&self) {
        // Wake connection attempts before locking their slots; otherwise an
        // unresponsive server could prevent shutdown from acquiring the lock.
        self.shutdown_signal.send_replace(true);
        for slot in &self.slots {
            let server = match tokio::time::timeout(CLOSE_TIMEOUT, slot.lock()).await {
                Ok(mut slot) => slot.take(),
                // A caller can retain a suspended connection future without
                // polling it. It will observe cancellation when polled again;
                // shutdown must not wait indefinitely for that caller.
                Err(_) => continue,
            };
            if let Some(server) = server {
                server.close().await;
            }
        }
    }

    /// Connect the direct servers a run with `policy` may use, reusing live
    /// shared connections. Servers disabled for the policy are not connected.
    async fn connect_for(&self, policy: &UserPolicy) -> Result<Vec<Arc<ConnectedMcpServer>>> {
        if *self.shutdown_signal.borrow() {
            anyhow::bail!("MCP pool is shutting down");
        }
        let wanted = self
            .configs
            .iter()
            .enumerate()
            .filter(|(_, config)| {
                config.transport != McpTransport::Responses
                    && !policy.is_mcp_server_disabled(&config.label)
            })
            .map(|(index, _)| self.server(index));
        futures::future::try_join_all(wanted).await
    }

    /// Like `connect_for`, but servers that fail to connect are reported by
    /// label instead of failing the rest.
    async fn connect_available_for(
        &self,
        policy: &UserPolicy,
    ) -> Result<(Vec<Arc<ConnectedMcpServer>>, Vec<McpServerFailure>)> {
        if *self.shutdown_signal.borrow() {
            anyhow::bail!("MCP pool is shutting down");
        }
        let wanted: Vec<usize> = self
            .configs
            .iter()
            .enumerate()
            .filter(|(_, config)| {
                config.transport != McpTransport::Responses
                    && !policy.is_mcp_server_disabled(&config.label)
            })
            .map(|(index, _)| index)
            .collect();
        let results =
            futures::future::join_all(wanted.iter().map(|&index| self.server(index))).await;
        if *self.shutdown_signal.borrow() {
            anyhow::bail!("MCP pool is shutting down");
        }
        let mut servers = Vec::new();
        let mut failures = Vec::new();
        for (index, result) in wanted.into_iter().zip(results) {
            match result {
                Ok(server) => servers.push(server),
                Err(error) => failures.push(McpServerFailure {
                    label: self.configs[index].label.clone(),
                    error: redact_error(&format!("{error:#}")),
                }),
            }
        }
        Ok((servers, failures))
    }

    async fn server(&self, index: usize) -> Result<Arc<ConnectedMcpServer>> {
        let mut shutdown = self.shutdown_signal.subscribe();
        if *shutdown.borrow() {
            anyhow::bail!("MCP pool is shutting down");
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => anyhow::bail!("MCP pool is shutting down"),
            result = self.connect_server(index) => result,
        }
    }

    async fn connect_server(&self, index: usize) -> Result<Arc<ConnectedMcpServer>> {
        let config = &self.configs[index];
        if !config.reuse_connection {
            return Ok(Arc::new(
                ConnectedMcpServer::connect(index, config, &self.oauth, CONNECTION_TIMEOUT).await?,
            ));
        }

        // Holding the slot lock while connecting makes concurrent runs wait
        // for one connection instead of starting several.
        let mut slot = self.slots[index].lock().await;
        if let Some(server) = slot.as_ref().filter(|server| server.is_healthy()) {
            return Ok(Arc::clone(server));
        }
        let server = Arc::new(
            ConnectedMcpServer::connect(index, config, &self.oauth, CONNECTION_TIMEOUT).await?,
        );
        *slot = Some(Arc::clone(&server));
        Ok(server)
    }
}

pub(crate) struct ConnectedMcpServer {
    config: McpServerConfig,
    service: McpClient,
    /// Tools permitted by the server config. User policy is applied per run.
    tools: Vec<DirectMcpTool>,
}

impl ConnectedMcpServer {
    async fn connect(
        server_index: usize,
        config: &McpServerConfig,
        oauth: &OAuthStore,
        timeout: Duration,
    ) -> Result<Self> {
        tokio::time::timeout(timeout, Self::connect_inner(server_index, config, oauth))
            .await
            .with_context(|| {
                format!(
                    "MCP server '{}' connection and tool discovery timed out after {} seconds",
                    config.label,
                    timeout.as_secs_f64()
                )
            })?
    }

    async fn connect_inner(
        server_index: usize,
        config: &McpServerConfig,
        oauth: &OAuthStore,
    ) -> Result<Self> {
        let service = open_service(config, oauth).await?;
        let listed_tools =
            service.peer().list_all_tools().await.with_context(|| {
                format!("failed to list tools from MCP server '{}'", config.label)
            })?;
        let mut tools = Vec::new();
        let mut function_names = HashSet::new();
        for (tool_index, tool) in listed_tools.into_iter().enumerate() {
            let name = tool.name.to_string();
            if !config.is_tool_enabled(&name) {
                continue;
            }
            let description = tool
                .description
                .as_deref()
                .unwrap_or("No description provided by the MCP server.")
                .to_string();
            let mut input_schema = tool.schema_as_json_value();
            if !input_schema.is_object() {
                input_schema = json!({
                    "type": "object",
                    "properties": {},
                });
            }
            tools.push(DirectMcpTool {
                function_name: function_name(
                    &config.label,
                    &name,
                    server_index,
                    tool_index,
                    &mut function_names,
                ),
                name,
                description,
                input_schema,
            });
        }

        Ok(Self {
            config: config.clone(),
            service,
            tools,
        })
    }

    fn is_healthy(&self) -> bool {
        !self.service.is_closed() && !self.service.peer().is_transport_closed()
    }

    async fn close(self: Arc<Self>) {
        match Arc::try_unwrap(self) {
            Ok(mut server) => {
                server.service.close_with_timeout(CLOSE_TIMEOUT).await.ok();
            }
            Err(shared) => shared.service.cancellation_token().cancel(),
        }
    }
}

/// Start and initialize an MCP session with a directly connected server.
async fn open_service(config: &McpServerConfig, oauth: &OAuthStore) -> Result<McpClient> {
    config.validate()?;
    let service = match config.transport {
        McpTransport::Responses => {
            anyhow::bail!(
                "MCP server '{}' is managed by the Responses API",
                config.label
            )
        }
        McpTransport::Stdio => {
            let command_name = config.command.as_deref().unwrap_or_default();
            let mut command = Command::new(command_name);
            command
                .args(&config.args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            if let Some(cwd) = &config.cwd {
                command.current_dir(cwd);
            }
            for (child_name, source_name) in &config.env_vars {
                let value = std::env::var(source_name).with_context(|| {
                    format!(
                        "MCP server '{}' requires environment variable '{}'",
                        config.label, source_name
                    )
                })?;
                command.env(child_name, value);
            }
            let transport = TokioChildProcess::new(command)
                .with_context(|| format!("failed to start stdio MCP server '{}'", config.label))?;
            client_config()
                .serve(transport)
                .await
                .with_context(|| format!("failed to initialize MCP server '{}'", config.label))?
        }
        McpTransport::StreamableHttp => {
            let url = config.url.as_deref().unwrap_or_default();
            let mut transport_config =
                StreamableHttpClientTransportConfig::with_uri(url.to_string());
            if let Some(authorization_env) = &config.authorization_env {
                let token = std::env::var(authorization_env).with_context(|| {
                    format!(
                        "MCP server '{}' requires environment variable '{}'",
                        config.label, authorization_env
                    )
                })?;
                transport_config = transport_config.auth_header(token);
            }
            let initialized = if config.oauth {
                let client = oauth.authorized_client(config).await?;
                client_config()
                    .serve(StreamableHttpClientTransport::with_client(
                        client,
                        transport_config,
                    ))
                    .await
            } else {
                client_config()
                    .serve(StreamableHttpClientTransport::from_config(transport_config))
                    .await
            };
            initialized.with_context(|| {
                    let label = &config.label;
                    if config.oauth {
                        format!("failed to initialize MCP server '{label}' (if its authorization expired, run `ano mcp login {label}`)")
                    } else {
                        format!("failed to initialize MCP server '{label}'")
                    }
                })?
        }
    };
    Ok(service)
}

/// Connect to `config` once and list every tool the server offers, including
/// tools that `allowed_tools` or `disabled_tools` turn off. A Responses-managed
/// server with a `url` is reached over Streamable HTTP, as the provider would.
pub async fn list_server_tools(
    config: &McpServerConfig,
    oauth: &OAuthStore,
) -> Result<Vec<McpToolCatalog>> {
    let mut config = config.clone();
    if config.transport == McpTransport::Responses {
        if config.url.is_none() {
            anyhow::bail!(
                "MCP server '{}' is reached through a tunnel that only the Responses API can use",
                config.label
            );
        }
        config.transport = McpTransport::StreamableHttp;
    }
    let list = async {
        let mut service = open_service(&config, oauth).await?;
        let listed =
            service.peer().list_all_tools().await.with_context(|| {
                format!("failed to list tools from MCP server '{}'", config.label)
            });
        service.close_with_timeout(CLOSE_TIMEOUT).await.ok();
        listed
    };
    let listed = tokio::time::timeout(CONNECTION_TIMEOUT, list)
        .await
        .with_context(|| {
            format!(
                "MCP server '{}' connection and tool discovery timed out after {} seconds",
                config.label,
                CONNECTION_TIMEOUT.as_secs()
            )
        })??;
    Ok(listed
        .into_iter()
        .map(|tool| McpToolCatalog {
            name: tool.name.to_string(),
            description: tool.description.map(|description| description.to_string()),
        })
        .collect())
}

/// The function name the model calls a tool by: `mcp__<label>__<tool>`, so
/// that the model can tell tools apart by name. Characters that function
/// names cannot contain become `_`. A name that is too long or already taken
/// falls back to one made of the server and tool positions.
fn function_name(
    label: &str,
    tool_name: &str,
    server_index: usize,
    tool_index: usize,
    used: &mut HashSet<String>,
) -> String {
    let tool_name: String = tool_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let readable = format!("{DIRECT_MCP_PREFIX}{label}__{tool_name}");
    let name = if readable.len() <= 64 && !used.contains(&readable) {
        readable
    } else {
        format!("{DIRECT_MCP_PREFIX}server_{server_index}__tool_{tool_index}")
    };
    used.insert(name.clone());
    name
}

#[async_trait]
impl DirectMcpServer for ConnectedMcpServer {
    fn config(&self) -> &McpServerConfig {
        &self.config
    }

    fn tools(&self) -> &[DirectMcpTool] {
        &self.tools
    }

    fn is_healthy(&self) -> bool {
        ConnectedMcpServer::is_healthy(self)
    }

    async fn call_tool(&self, tool_name: &str, arguments: Map<String, Value>) -> Result<Value> {
        let result = self
            .service
            .call_tool(CallToolRequestParams::new(tool_name.to_string()).with_arguments(arguments))
            .await?;
        serde_json::to_value(result).context("failed to serialize MCP tool result")
    }
}

#[async_trait]
impl McpGateway for McpPool {
    fn configs(&self) -> &[McpServerConfig] {
        &self.configs
    }

    async fn connect(&self, policy: &UserPolicy) -> Result<Vec<Arc<dyn DirectMcpServer>>> {
        Ok(self
            .connect_for(policy)
            .await?
            .into_iter()
            .map(|server| server as Arc<dyn DirectMcpServer>)
            .collect())
    }

    async fn connect_available(
        &self,
        policy: &UserPolicy,
    ) -> Result<(Vec<Arc<dyn DirectMcpServer>>, Vec<McpServerFailure>)> {
        let (servers, failures) = self.connect_available_for(policy).await?;
        Ok((
            servers
                .into_iter()
                .map(|server| server as Arc<dyn DirectMcpServer>)
                .collect(),
            failures,
        ))
    }

    async fn shutdown(&self) {
        McpPool::shutdown(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::{function_name, ConnectedMcpServer, McpPool};
    use crate::{
        domain::{
            mcp::{McpApprovalMode, McpServerConfig, McpTransport},
            policy::UserPolicy,
        },
        infrastructure::mcp_oauth::OAuthStore,
    };
    use std::{collections::HashSet, sync::Arc, time::Duration};
    use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

    fn stdio_server(label: &str) -> McpServerConfig {
        McpServerConfig {
            label: label.into(),
            transport: McpTransport::Stdio,
            url: None,
            tunnel_id: None,
            command: Some("ano-test-command-that-does-not-exist".into()),
            args: vec![],
            cwd: None,
            env_vars: Default::default(),
            description: None,
            authorization_env: None,
            oauth: false,
            oauth_scopes: None,
            allowed_tools: None,
            disabled_tools: vec![],
            tool_catalog: None,
            require_approval: McpApprovalMode::Always,
            reuse_connection: true,
        }
    }

    #[test]
    fn function_names_are_readable_and_unique() {
        let mut used = HashSet::new();
        let mut name = |tool: &str, index| function_name("annict", tool, 2, index, &mut used);
        assert_eq!(
            name("annict_update_status", 0),
            "mcp__annict__annict_update_status"
        );
        assert_eq!(name("search.works", 1), "mcp__annict__search_works");
        assert_eq!(name("search_works", 2), "mcp__server_2__tool_2");
        assert_eq!(name(&"x".repeat(60), 3), "mcp__server_2__tool_3");
    }

    #[tokio::test]
    async fn does_not_connect_servers_disabled_for_the_policy() {
        let pool = McpPool::new(vec![stdio_server("files")]);
        let policy = UserPolicy::new(vec!["mcp:files".into()], None);

        let servers = pool.connect_for(&policy).await.unwrap();
        assert!(servers.is_empty());
        assert!(pool.connected_servers().await.is_empty());
    }

    #[tokio::test]
    async fn failed_connection_is_not_cached() {
        let pool = McpPool::new(vec![stdio_server("files")]);

        assert!(pool.connect_for(&UserPolicy::default()).await.is_err());
        assert!(pool.slots[0].lock().await.is_none());
    }

    #[tokio::test]
    async fn servers_that_fail_to_connect_are_reported_not_fatal() {
        let pool = McpPool::new(vec![stdio_server("files"), stdio_server("disabled")]);
        let policy = UserPolicy::new(vec!["mcp:disabled:*".into()], None);

        let (servers, failures) = pool.connect_available_for(&policy).await.unwrap();
        assert!(servers.is_empty());
        assert_eq!(failures.len(), 1, "disabled servers are not connected");
        assert_eq!(failures[0].label, "files");
        assert!(failures[0].error.contains("files"), "{}", failures[0].error);
        assert!(pool.slots[0].lock().await.is_none());

        pool.shutdown().await;
        assert!(pool.connect_available_for(&policy).await.is_err());
    }

    #[tokio::test]
    async fn clients_ask_for_a_protocol_version_with_a_handshake() {
        use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post};
        use serde_json::{json, Value};

        // Answers like a server that supports 2025-11-25 at most.
        async fn handle(
            State(seen): State<Arc<std::sync::Mutex<Vec<Value>>>>,
            axum::Json(body): axum::Json<Value>,
        ) -> axum::response::Response {
            seen.lock().unwrap().push(body.clone());
            let result = match body["method"].as_str() {
                Some("initialize") => json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mock", "version": "1"}
                }),
                Some("tools/list") => json!({"tools": []}),
                _ => return StatusCode::ACCEPTED.into_response(),
            };
            axum::Json(json!({"jsonrpc": "2.0", "id": body["id"], "result": result}))
                .into_response()
        }
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/mcp", post(handle))
            .with_state(Arc::clone(&seen));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = stdio_server("mock");
        config.transport = McpTransport::StreamableHttp;
        config.command = None;
        config.url = Some(format!("http://{}/mcp", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let pool = McpPool::new(vec![config]);
        let servers = pool.connect_for(&UserPolicy::default()).await.unwrap();
        assert_eq!(servers.len(), 1);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[0]["method"], "initialize");
        assert_eq!(seen[0]["params"]["protocolVersion"], "2025-11-25");
        assert!(seen
            .iter()
            .any(|message| message["method"] == "notifications/initialized"));
        pool.shutdown().await;
        server.abort();
    }

    #[test]
    fn connection_errors_lose_credentials_and_length() {
        let error = "error sending request for url (https://user:secret@mcp.example.com/mcp/?apiKey=tvly-secret#frag): refused";
        let redacted = super::redact_error(error);
        assert_eq!(
            redacted,
            "error sending request for url (https://mcp.example.com/mcp/): refused"
        );
        assert_eq!(
            super::redact_error(
                "rejected Authorization: Bearer ghp_abc123, token=xyz; api_key: k1"
            ),
            "rejected Authorization: [redacted], token=[redacted]; api_key: [redacted]"
        );
        assert_eq!(
            super::redact_error("failed: access_token=gho_1 client-secret: cs2 refresh_token x3"),
            "failed: access_token=[redacted] client-secret: [redacted] refresh_token [redacted]"
        );
        assert_eq!(
            super::redact_error("HTTP 401: invalid bearer abc.def"),
            "HTTP 401: invalid bearer [redacted]"
        );
        let long = super::redact_error(&"x".repeat(1000));
        assert_eq!(long.chars().count(), super::MAX_FAILURE_CHARS + 1);
    }

    async fn stalled_http_server() -> (McpServerConfig, JoinHandle<()>, oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = stdio_server("stalled");
        config.transport = McpTransport::StreamableHttp;
        config.command = None;
        config.url = Some(format!("http://{}/mcp", listener.local_addr().unwrap()));
        let (accepted, ready) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (_connection, _) = listener.accept().await.unwrap();
            let _ = accepted.send(());
            std::future::pending::<()>().await;
        });
        (config, server, ready)
    }

    #[tokio::test]
    async fn connection_to_an_unresponsive_server_times_out() {
        let (config, server, _) = stalled_http_server().await;
        let result = ConnectedMcpServer::connect(
            0,
            &config,
            &OAuthStore::default_location(),
            Duration::from_millis(20),
        )
        .await;
        server.abort();

        let error = result.err().expect("connection should time out");
        assert!(error
            .to_string()
            .contains("connection and tool discovery timed out"));
    }

    #[tokio::test]
    async fn shutdown_cancels_connection_attempts_without_waiting_for_their_slot() {
        let (config, server, ready) = stalled_http_server().await;
        let pool = Arc::new(McpPool::new(vec![config]));
        let task_pool = Arc::clone(&pool);
        let task = tokio::spawn(async move {
            task_pool
                .connect_for(&UserPolicy::default())
                .await
                .map(|_| ())
        });
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), pool.shutdown())
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        server.abort();
        assert!(error.to_string().contains("shutting down"));
        assert!(pool.connected_servers().await.is_empty());
        assert!(pool.connect_for(&UserPolicy::default()).await.is_err());
    }
}
