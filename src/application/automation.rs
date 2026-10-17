//! The ports of automation: sources that turn requests posted from outside
//! a conversation into events, and notifiers that tell the user. Both are
//! registered by `kind` in an `AutomationRegistry`, so a harness or a
//! library user adds new kinds without changing the code that handles
//! events.

use crate::domain::automation::{AutomationEvent, Priority, Trust};
use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

/// What the user is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    /// Markdown text.
    pub message: String,
    pub priority: Priority,
    /// A page to open, such as the issue the notification is about.
    pub url: Option<String>,
    /// The automation rule it comes from.
    pub rule: Option<String>,
}

/// Delivers notifications to one destination.
#[async_trait]
pub trait Notifier: Send + Sync {
    async fn send(&self, notification: &Notification) -> Result<()>;
}

/// A configured notifier with its name in `[automation.notifiers]`.
#[derive(Clone)]
pub struct NamedNotifier {
    pub name: String,
    pub notifier: Arc<dyn Notifier>,
}

/// Whether an event handed to automation is new.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// It was recorded and handled.
    New,
    /// It was handled before.
    Duplicate,
    /// It could not be recorded, so it was not handled.
    Failed(String),
}

/// How a source that takes posted requests has them authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostAuthentication {
    /// The server checks ano's signature (`X-Ano-Timestamp` and
    /// `X-Ano-Signature` over `<timestamp>.event:<name>:<body>`) and refuses
    /// replays before the source sees the request.
    Server,
    /// The source checks the request itself in `receive`, for services that
    /// sign their requests their own way (GitHub, Slack, Stripe, ...). The
    /// source is then also the one to refuse replays.
    Source,
}

/// A request posted to `POST /events/<name>`.
#[derive(Debug, Clone, Copy)]
pub struct PostedRequest<'a> {
    /// The name of the source it was posted to.
    pub source: &'a str,
    /// Header names in lower case, with the values that are text.
    pub headers: &'a BTreeMap<String, String>,
    pub body: &'a [u8],
}

impl PostedRequest<'_> {
    /// The value of the header `name` (in any case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

/// Why a posted request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The source could not authenticate it (401).
    Unauthorized(String),
    /// It is not an event the source can read (400).
    Invalid(String),
}

impl Rejection {
    pub fn invalid(error: impl fmt::Display) -> Self {
        Self::Invalid(error.to_string())
    }
}

/// Where events come from: requests posted to `POST /events/<name>`, which
/// the source reads in its own format. Data that has to be fetched is
/// better fetched by a run, with MCP tools, that a posted event starts.
#[async_trait]
pub trait EventSource: Send + Sync {
    /// The trust of its events when nothing raises it. Rules that can run
    /// tasks for an untrusted source are checked more strictly.
    fn trust(&self) -> Trust;

    /// One line on what it receives, for `ano automation list`.
    fn describe(&self) -> String;

    /// Who authenticates the posted requests: by default the server, with
    /// ano's signature.
    fn authentication(&self) -> PostAuthentication {
        PostAuthentication::Server
    }

    /// The event in a posted request. With `PostAuthentication::Server`,
    /// the server has authenticated it; with `Source`, this must.
    fn receive(&self, request: &PostedRequest<'_>) -> Result<AutomationEvent, Rejection>;
}

/// The settings of one configured source or notifier.
#[derive(Debug, Clone)]
pub struct KindSpec<'a> {
    /// Its name in `[automation.sources]` or `[automation.notifiers]`.
    pub name: &'a str,
    /// The `trust` setting of a source; unset for notifiers.
    pub trust: Option<Trust>,
    /// Its other settings, as a JSON object, for the kind to read and
    /// check.
    pub options: &'a Value,
}

pub type SourceFactory = Arc<dyn Fn(&KindSpec<'_>) -> Result<Arc<dyn EventSource>> + Send + Sync>;
pub type NotifierFactory = Arc<dyn Fn(&KindSpec<'_>) -> Result<Arc<dyn Notifier>> + Send + Sync>;

/// The kinds of sources and notifiers that configuration can name.
#[derive(Clone, Default)]
pub struct AutomationRegistry {
    sources: BTreeMap<String, SourceFactory>,
    notifiers: BTreeMap<String, NotifierFactory>,
}

impl fmt::Debug for AutomationRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AutomationRegistry")
            .field("sources", &self.sources.keys().collect::<Vec<_>>())
            .field("notifiers", &self.notifiers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl AutomationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Let `[automation.sources]` use `kind`, built by `factory`. A kind
    /// registered again replaces the earlier one.
    pub fn register_source(
        &mut self,
        kind: &str,
        factory: impl Fn(&KindSpec<'_>) -> Result<Arc<dyn EventSource>> + Send + Sync + 'static,
    ) {
        self.sources.insert(kind.to_string(), Arc::new(factory));
    }

    /// Let `[automation.notifiers]` use `kind`, built by `factory`.
    pub fn register_notifier(
        &mut self,
        kind: &str,
        factory: impl Fn(&KindSpec<'_>) -> Result<Arc<dyn Notifier>> + Send + Sync + 'static,
    ) {
        self.notifiers.insert(kind.to_string(), Arc::new(factory));
    }

    pub fn source_kinds(&self) -> impl Iterator<Item = &str> {
        self.sources.keys().map(String::as_str)
    }

    pub fn notifier_kinds(&self) -> impl Iterator<Item = &str> {
        self.notifiers.keys().map(String::as_str)
    }

    pub fn build_source(&self, kind: &str, spec: &KindSpec<'_>) -> Result<Arc<dyn EventSource>> {
        match self.sources.get(kind) {
            Some(factory) => factory(spec),
            None => bail!(
                "unknown source kind '{kind}'; known kinds: {}",
                self.source_kinds().collect::<Vec<_>>().join(", ")
            ),
        }
    }

    pub fn build_notifier(&self, kind: &str, spec: &KindSpec<'_>) -> Result<Arc<dyn Notifier>> {
        match self.notifiers.get(kind) {
            Some(factory) => factory(spec),
            None => bail!(
                "unknown notifier kind '{kind}'; known kinds: {}",
                self.notifier_kinds().collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

/// The current Unix time in seconds.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Quiet;

    #[async_trait]
    impl EventSource for Quiet {
        fn trust(&self) -> Trust {
            Trust::Trusted
        }

        fn describe(&self) -> String {
            "quiet".into()
        }

        fn receive(&self, _request: &PostedRequest<'_>) -> Result<AutomationEvent, Rejection> {
            Err(Rejection::invalid("nothing to read"))
        }
    }

    #[test]
    fn kinds_are_built_by_their_factories_and_unknown_kinds_name_the_known_ones() {
        let mut registry = AutomationRegistry::new();
        registry.register_source("quiet", |spec| {
            if spec.options.get("loud").is_some() {
                bail!("loud is not a setting of quiet");
            }
            Ok(Arc::new(Quiet))
        });
        let options = json!({});
        let spec = KindSpec {
            name: "q",
            trust: None,
            options: &options,
        };
        assert_eq!(
            registry.build_source("quiet", &spec).unwrap().describe(),
            "quiet"
        );
        let error = registry
            .build_source("rss", &spec)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("unknown source kind 'rss'") && error.contains("quiet"),
            "{error}"
        );
        let loud = json!({"loud": true});
        let spec = KindSpec {
            options: &loud,
            ..spec
        };
        assert!(registry.build_source("quiet", &spec).is_err());
    }
}
