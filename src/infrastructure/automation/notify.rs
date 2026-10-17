//! The built-in notifiers: ntfy topics, Discord and Slack incoming
//! webhooks, and signed JSON posts to any HTTP endpoint.

use super::{error_excerpt, secure_url};
use crate::{
    application::automation::{Notification, Notifier},
    domain::automation::Priority,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::{redirect::Policy, Url};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DISCORD_MAX_CHARS: usize = 2000;
const SLACK_MAX_CHARS: usize = 3500;
const MESSAGE_MAX_CHARS: usize = 4000;

/// The built-in kinds of notifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpNotifierKind {
    /// An ntfy topic URL, such as `https://ntfy.sh/my-topic`.
    Ntfy,
    /// A Discord incoming webhook URL.
    Discord,
    /// A Slack incoming webhook URL.
    Slack,
    /// Any endpoint, which receives the notification as JSON, signed like
    /// requests to `ano serve` when `secret_env` is set.
    Webhook,
}

impl HttpNotifierKind {
    pub const ALL: [Self; 4] = [Self::Ntfy, Self::Discord, Self::Slack, Self::Webhook];

    /// The `kind` of `[automation.notifiers]` that names it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ntfy => "ntfy",
            Self::Discord => "discord",
            Self::Slack => "slack",
            Self::Webhook => "webhook",
        }
    }
}

/// The settings of a built-in notifier, besides `kind`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpNotifierSettings {
    /// The URL. Webhook URLs of Discord and Slack are secrets: give those
    /// with `url_env` instead.
    #[serde(default)]
    pub url: Option<String>,
    /// The environment variable holding the URL.
    #[serde(default)]
    pub url_env: Option<String>,
    /// ntfy: the environment variable holding an access token.
    #[serde(default)]
    pub token_env: Option<String>,
    /// webhook: the environment variable holding the HMAC secret.
    #[serde(default)]
    pub secret_env: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    15
}

impl HttpNotifierSettings {
    pub fn validate(&self, kind: HttpNotifierKind) -> Result<()> {
        match (&self.url, &self.url_env) {
            (Some(_), Some(_)) => bail!("set either url or url_env, not both"),
            (None, None) => bail!("url or url_env is required"),
            (Some(url), None) => {
                check_url(url)?;
                if matches!(kind, HttpNotifierKind::Discord | HttpNotifierKind::Slack) {
                    bail!("the webhook URL of Discord and Slack is a secret; set url_env instead of url");
                }
            }
            (None, Some(name)) if name.trim().is_empty() => bail!("url_env must not be empty"),
            _ => {}
        }
        if self.token_env.is_some() && kind != HttpNotifierKind::Ntfy {
            bail!("token_env is only for kind = \"ntfy\"");
        }
        if self.secret_env.is_some() && kind != HttpNotifierKind::Webhook {
            bail!("secret_env is only for kind = \"webhook\"");
        }
        if self.timeout_secs == 0 {
            bail!("timeout_secs must be greater than zero");
        }
        Ok(())
    }
}

/// Sends notifications over HTTP to one configured destination.
pub struct HttpNotifier {
    kind: HttpNotifierKind,
    url: Url,
    token: Option<String>,
    secret: Option<Vec<u8>>,
    http: reqwest::Client,
}

impl HttpNotifier {
    /// The notifier of `kind`, reading the variables it names with
    /// `get_env`.
    pub fn from_settings(
        kind: HttpNotifierKind,
        settings: &HttpNotifierSettings,
        get_env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        settings.validate(kind)?;
        let read = |variable: &str| -> Result<String> {
            match get_env(variable) {
                Some(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
                Some(_) => bail!("{variable} is set but empty"),
                None => bail!("{variable} is not set"),
            }
        };
        let url = match (&settings.url, &settings.url_env) {
            (Some(url), _) => url.clone(),
            (None, Some(variable)) => read(variable)?,
            (None, None) => unreachable!("validated"),
        };
        let url = check_url(&url)?;
        if kind == HttpNotifierKind::Ntfy && ntfy_topic(&url).is_none() {
            bail!("an ntfy URL must end with the topic, as in https://ntfy.sh/my-topic");
        }
        let http = reqwest::Client::builder()
            // Redirects could carry the token or the message elsewhere.
            .redirect(Policy::none())
            .timeout(Duration::from_secs(settings.timeout_secs))
            .user_agent(concat!("ano/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            kind,
            url,
            token: settings.token_env.as_deref().map(read).transpose()?,
            secret: settings
                .secret_env
                .as_deref()
                .map(read)
                .transpose()?
                .map(String::into_bytes),
            http,
        })
    }
}

#[async_trait]
impl Notifier for HttpNotifier {
    async fn send(&self, notification: &Notification) -> Result<()> {
        let (url, body) = match self.kind {
            HttpNotifierKind::Ntfy => {
                let (server, topic) = ntfy_topic(&self.url).context("invalid ntfy URL")?;
                let mut body = json!({
                    "topic": topic,
                    "title": notification.title,
                    "message": cut(&notification.message, MESSAGE_MAX_CHARS),
                    "priority": match notification.priority {
                        Priority::Low => 2,
                        Priority::Normal => 3,
                        Priority::High => 4,
                    },
                    "markdown": true,
                });
                if let Some(click) = notification.url.as_deref().filter(|url| is_http(url)) {
                    body["click"] = json!(click);
                }
                (server, body)
            }
            HttpNotifierKind::Discord => (
                self.url.clone(),
                json!({
                    "content": cut(&chat_text(notification, "**", |text| text.to_string()), DISCORD_MAX_CHARS),
                    // Text the agent passes on must never ping anyone.
                    "allowed_mentions": {"parse": []},
                }),
            ),
            HttpNotifierKind::Slack => (
                self.url.clone(),
                json!({"text": cut(&chat_text(notification, "*", slack_escape), SLACK_MAX_CHARS)}),
            ),
            HttpNotifierKind::Webhook => (
                self.url.clone(),
                json!({
                    "title": notification.title,
                    "message": cut(&notification.message, MESSAGE_MAX_CHARS),
                    "priority": notification.priority,
                    "url": notification.url,
                    "rule": notification.rule,
                }),
            ),
        };
        let bytes = serde_json::to_vec(&body)?;
        let mut request = self
            .http
            .post(url)
            .header("content-type", "application/json");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(secret) = &self.secret {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or_default()
                .to_string();
            let mut mac =
                Hmac::<Sha256>::new_from_slice(secret).context("invalid notifier secret")?;
            mac.update(timestamp.as_bytes());
            mac.update(b".");
            mac.update(&bytes);
            request = request.header("x-ano-timestamp", timestamp).header(
                "x-ano-signature",
                format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
            );
        }
        // The URL can hold a secret, so errors leave it out.
        let response = request
            .body(bytes)
            .send()
            .await
            .map_err(|error| error.without_url())
            .context("failed to send the notification")?;
        let status = response.status();
        if !status.is_success() {
            let excerpt = error_excerpt(response).await;
            bail!(
                "the notifier answered {status}{}",
                if excerpt.is_empty() {
                    String::new()
                } else {
                    format!(": {excerpt}")
                }
            );
        }
        Ok(())
    }
}

/// The text of a chat message: the title in bold, the message, and the link.
fn chat_text(notification: &Notification, bold: &str, escape: fn(&str) -> String) -> String {
    let mut text = format!(
        "{bold}{}{bold}\n{}",
        escape(&notification.title),
        escape(&notification.message)
    );
    if let Some(url) = notification.url.as_deref().filter(|url| is_http(url)) {
        text.push('\n');
        text.push_str(&escape(url));
    }
    text
}

/// Slack reads `<...>` as links and mentions such as `<!channel>`.
fn slack_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The server URL to publish to and the topic of an ntfy topic URL.
fn ntfy_topic(url: &Url) -> Option<(Url, String)> {
    let mut segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let topic = segments.pop()?.to_string();
    let mut server = url.clone();
    server.set_query(None);
    server.set_path(&format!("{}/", segments.join("/")));
    Some((server, topic))
}

fn check_url(url: &str) -> Result<Url> {
    secure_url(url, "the notifier URL")
}

fn is_http(url: &str) -> bool {
    Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

fn cut(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut result: String = text.chars().take(limit.saturating_sub(1)).collect();
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, http::HeaderMap, routing::post, Router};
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    type Received = Arc<Mutex<Vec<(String, HeaderMap, Value)>>>;

    async fn record(
        State(received): State<Received>,
        uri: axum::http::Uri,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> &'static str {
        received.lock().unwrap().push((
            uri.path().to_string(),
            headers,
            serde_json::from_slice(&body).unwrap(),
        ));
        "ok"
    }

    async fn server() -> (String, Received) {
        let received: Received = Arc::default();
        let app = Router::new()
            .route("/", post(record))
            .route("/{*path}", post(record))
            .with_state(Arc::clone(&received));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), received)
    }

    /// The notifier of the TOML `text`, whose `kind` picks the kind.
    fn build(text: &str, get_env: impl Fn(&str) -> Option<String>) -> Result<HttpNotifier> {
        let mut table: toml::Table = toml::from_str(text).unwrap();
        let kind = table.remove("kind").unwrap();
        let kind = HttpNotifierKind::ALL
            .into_iter()
            .find(|candidate| kind.as_str() == Some(candidate.name()))
            .unwrap();
        let settings: HttpNotifierSettings = table.try_into()?;
        HttpNotifier::from_settings(kind, &settings, get_env)
    }

    fn notification() -> Notification {
        Notification {
            title: "新しい Issue".into(),
            message: "起動時に落ちる <!channel> @everyone".into(),
            priority: Priority::High,
            url: Some("https://github.com/o/r/issues/1".into()),
            rule: Some("issues".into()),
        }
    }

    #[tokio::test]
    async fn ntfy_publishes_json_to_the_server_with_the_token() {
        let (base, received) = server().await;
        let notifier = build(
            &format!("kind = 'ntfy'\nurl = '{base}/alerts'\ntoken_env = 'TOKEN'"),
            |name| (name == "TOKEN").then(|| "tk".to_string()),
        )
        .unwrap();
        notifier.send(&notification()).await.unwrap();
        let (path, headers, body) = received.lock().unwrap().remove(0);
        assert_eq!(path, "/");
        assert_eq!(headers["authorization"], "Bearer tk");
        assert_eq!(body["topic"], "alerts");
        assert_eq!(body["title"], "新しい Issue");
        assert_eq!(body["priority"], 4);
        assert_eq!(body["click"], "https://github.com/o/r/issues/1");
    }

    #[tokio::test]
    async fn chat_webhooks_never_mention_anyone() {
        let (base, received) = server().await;
        for kind in ["discord", "slack"] {
            let notifier = build(&format!("kind = '{kind}'\nurl_env = 'URL'"), |_| {
                Some(format!("{base}/{kind}"))
            })
            .unwrap();
            notifier.send(&notification()).await.unwrap();
        }
        let received = received.lock().unwrap();
        let discord = &received[0].2;
        assert_eq!(discord["allowed_mentions"]["parse"], json!([]));
        assert!(discord["content"]
            .as_str()
            .unwrap()
            .starts_with("**新しい Issue**\n"));
        let slack = received[1].2["text"].as_str().unwrap();
        assert!(slack.contains("&lt;!channel&gt;"), "{slack}");
        assert!(!slack.contains("<!channel>"));
    }

    #[tokio::test]
    async fn webhooks_are_signed_with_the_secret() {
        let (base, received) = server().await;
        let notifier = build(
            &format!("kind = 'webhook'\nurl = '{base}/hook'\nsecret_env = 'S'"),
            |_| Some("secret".into()),
        )
        .unwrap();
        notifier.send(&notification()).await.unwrap();
        let (_, headers, body) = received.lock().unwrap().remove(0);
        let timestamp = headers["x-ano-timestamp"].to_str().unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(format!("{timestamp}.").as_bytes());
        mac.update(&serde_json::to_vec(&body).unwrap());
        let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert_eq!(headers["x-ano-signature"], expected.as_str());
        assert_eq!(body["rule"], "issues");
        assert_eq!(body["priority"], "high");
    }

    #[tokio::test]
    async fn failures_leave_out_the_url() {
        let notifier = build("kind = 'discord'\nurl_env = 'URL'", |_| {
            Some("http://127.0.0.1:9/api/webhooks/secret-token".into())
        })
        .unwrap();
        let error = format!("{:#}", notifier.send(&notification()).await.unwrap_err());
        assert!(error.contains("failed to send"), "{error}");
        assert!(!error.contains("secret-token"), "{error}");
    }

    #[test]
    fn settings_keep_secret_urls_out_of_the_config_file() {
        let invalid = |text: &str| build(text, |_| None).err().unwrap().to_string();
        assert!(
            invalid("kind = 'discord'\nurl = 'https://discord.com/api/webhooks/1/x'")
                .contains("url_env")
        );
        assert!(invalid("kind = 'ntfy'").contains("required"));
        assert!(invalid("kind = 'ntfy'\nurl = 'ftp://x/y'").contains("http"));
        assert!(invalid("kind = 'slack'\nurl_env = 'U'\ntoken_env = 'T'").contains("ntfy"));
        assert!(
            invalid("kind = 'ntfy'\nurl = 'https://ntfy.sh/a'\nhost = 'x'")
                .contains("unknown field")
        );
        assert!(invalid("kind = 'slack'\nurl_env = 'U'").contains("U is not set"));
        assert!(invalid("kind = 'ntfy'\nurl = 'https://ntfy.sh/'").contains("topic"));
    }
}
