//! The `webhook` source: events posted to `POST /events/<name>` as JSON,
//! such as reports a service sends. Requests are signed like requests to
//! `ano serve`: with the server's secret, which the server checks, or with
//! the source's own `secret_env`, which the source checks, so that a poster
//! holds no key to the rest of the server.

use super::options;
use crate::{
    application::automation::{
        unix_now, EventSource, KindSpec, PostAuthentication, PostedRequest, Rejection,
    },
    domain::automation::{
        AutomationEvent, Trust, MAX_EVENT_FIELD_CHARS, MAX_EVENT_TITLE_CHARS, MAX_EVENT_URL_CHARS,
    },
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_BODY_BYTES: usize = 100_000;
const SIGNATURE_HEADER: &str = "x-ano-signature";
const TIMESTAMP_HEADER: &str = "x-ano-timestamp";
const DEFAULT_TOLERANCE_SECS: u64 = 300;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookSettings {
    /// The environment variable holding this source's own secret. Without
    /// one, posts are signed with the server's `webhook.secret_env`.
    #[serde(default)]
    secret_env: Option<String>,
    /// With `secret_env`: how far `X-Ano-Timestamp` may be from now.
    #[serde(default)]
    signature_tolerance_secs: Option<u64>,
}

/// A posted event.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostedEvent {
    /// What happened; `report` when unset.
    #[serde(default)]
    kind: Option<String>,
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    author: Option<String>,
    /// Identifies the event, so a repeated post is handled once.
    #[serde(default)]
    key: Option<String>,
}

pub struct WebhookSource {
    name: String,
    trust: Trust,
    /// This source's own secret and timestamp tolerance.
    secret: Option<(Vec<u8>, u64)>,
}

impl WebhookSource {
    pub fn new(spec: &KindSpec<'_>) -> Result<Self> {
        Self::with_env(spec, |name| std::env::var(name).ok())
    }

    fn with_env(spec: &KindSpec<'_>, get_env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let settings: WebhookSettings = options(spec)?;
        let secret = match &settings.secret_env {
            Some(variable) => match get_env(variable) {
                Some(secret) if !secret.is_empty() => Some((
                    secret.into_bytes(),
                    settings
                        .signature_tolerance_secs
                        .unwrap_or(DEFAULT_TOLERANCE_SECS),
                )),
                Some(_) => bail!("{variable} is set but empty"),
                None => bail!("{variable} is not set"),
            },
            None if settings.signature_tolerance_secs.is_some() => {
                bail!("signature_tolerance_secs needs secret_env")
            }
            None => None,
        };
        if secret
            .as_ref()
            .is_some_and(|(_, tolerance)| *tolerance == 0)
        {
            bail!("signature_tolerance_secs must be greater than zero");
        }
        Ok(Self {
            name: spec.name.to_string(),
            // A signature proves who posted, not who wrote what was posted:
            // the content counts as the user's own only when set so.
            trust: spec.trust.unwrap_or(Trust::Untrusted),
            secret,
        })
    }

    /// Check the source's own signature over `<timestamp>.event:<name>:<body>`.
    fn verify(&self, request: &PostedRequest<'_>) -> Result<(), Rejection> {
        let Some((secret, tolerance)) = &self.secret else {
            return Ok(());
        };
        let unauthorized = |message: &str| Err(Rejection::Unauthorized(message.into()));
        let Some(timestamp) = request.header(TIMESTAMP_HEADER) else {
            return unauthorized("missing X-Ano-Timestamp");
        };
        let Ok(sent) = timestamp.parse::<u64>() else {
            return unauthorized("invalid X-Ano-Timestamp");
        };
        if unix_now().abs_diff(sent) > *tolerance {
            return unauthorized("X-Ano-Timestamp is outside the accepted window");
        }
        let Some(signature) = request.header(SIGNATURE_HEADER) else {
            return unauthorized("missing X-Ano-Signature");
        };
        let Ok(provided) = hex::decode(signature.strip_prefix("sha256=").unwrap_or(signature))
        else {
            return unauthorized("invalid webhook signature");
        };
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
            return unauthorized("invalid webhook secret");
        };
        mac.update(format!("{timestamp}.event:{}:", request.source).as_bytes());
        mac.update(request.body);
        if mac.verify_slice(&provided).is_err() {
            return unauthorized("invalid webhook signature");
        }
        Ok(())
    }

    fn parse(&self, request: &PostedRequest<'_>) -> Result<AutomationEvent> {
        let posted: PostedEvent = serde_json::from_slice(request.body).context("invalid JSON")?;
        let title = posted.title.trim();
        if title.is_empty() || title.chars().count() > MAX_EVENT_TITLE_CHARS {
            bail!("title must not be empty and at most {MAX_EVENT_TITLE_CHARS} characters");
        }
        if posted.body.len() > MAX_BODY_BYTES {
            bail!("body is too long");
        }
        let kind = posted.kind.unwrap_or_else(|| "report".into());
        let short = |text: Option<&str>| {
            text.is_none_or(|text| {
                !text.trim().is_empty() && text.chars().count() <= MAX_EVENT_FIELD_CHARS
            })
        };
        if !short(Some(&kind)) || !short(posted.key.as_deref()) || !short(posted.author.as_deref())
        {
            bail!(
                "kind, key, and author must not be empty and at most {MAX_EVENT_FIELD_CHARS} characters"
            );
        }
        if posted
            .url
            .as_deref()
            .is_some_and(|url| url.chars().count() > MAX_EVENT_URL_CHARS)
        {
            bail!("url is too long");
        }
        // Without a key, the signed time and the body name the event, so
        // the same request sent again, even after a restart, is handled
        // once, while a new post of the same text is a new event. Unsigned
        // posts (allowed on loopback only) have no time: each is new.
        let key = match (posted.key, request.header(TIMESTAMP_HEADER)) {
            (Some(key), _) => key,
            (None, Some(timestamp)) => {
                let mut digest = Sha256::new();
                digest.update(timestamp.as_bytes());
                digest.update(b".");
                digest.update(request.body);
                format!("post:{}", hex::encode(digest.finalize()))
            }
            (None, None) => format!("post:{}", uuid::Uuid::new_v4()),
        };
        Ok(AutomationEvent {
            source: request.source.to_string(),
            kind,
            key,
            title: title.to_string(),
            body: posted.body,
            url: posted.url,
            author: posted.author,
            trust: self.trust,
        })
    }
}

#[async_trait]
impl EventSource for WebhookSource {
    fn trust(&self) -> Trust {
        self.trust
    }

    fn describe(&self) -> String {
        format!(
            "receives POST /events/{} signed with {}",
            self.name,
            if self.secret.is_some() {
                "its own secret"
            } else {
                "the server's secret"
            }
        )
    }

    fn authentication(&self) -> PostAuthentication {
        if self.secret.is_some() {
            PostAuthentication::Source
        } else {
            PostAuthentication::Server
        }
    }

    fn receive(&self, request: &PostedRequest<'_>) -> Result<AutomationEvent, Rejection> {
        self.verify(request)?;
        self.parse(request)
            .map_err(|error| Rejection::Invalid(format!("{error:#}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn source(trust: Option<Trust>, options: serde_json::Value) -> Result<WebhookSource> {
        WebhookSource::with_env(
            &KindSpec {
                name: "reports",
                trust,
                options: &options,
            },
            |name| (name == "REPORTS_SECRET").then(|| "own-secret".to_string()),
        )
    }

    fn post(
        source: &WebhookSource,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Result<AutomationEvent, Rejection> {
        let headers: BTreeMap<String, String> = headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect();
        source.receive(&PostedRequest {
            source: "reports",
            headers: &headers,
            body,
        })
    }

    fn receive(event: serde_json::Value) -> Result<AutomationEvent, Rejection> {
        post(
            &source(None, json!({})).unwrap(),
            &[],
            &serde_json::to_vec(&event).unwrap(),
        )
    }

    fn signed(secret: &str, timestamp: u64, body: &[u8]) -> Vec<(&'static str, String)> {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{timestamp}.event:reports:").as_bytes());
        mac.update(body);
        vec![
            (TIMESTAMP_HEADER, timestamp.to_string()),
            (
                SIGNATURE_HEADER,
                format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
            ),
        ]
    }

    #[test]
    fn posts_become_untrusted_events_of_the_source_unless_set_trusted() {
        let event = receive(json!({"title": " Weekly ", "body": "DAU 700", "key": "w42"})).unwrap();
        assert_eq!(
            (
                event.source.as_str(),
                event.kind.as_str(),
                event.key.as_str(),
                event.title.as_str()
            ),
            ("reports", "report", "w42", "Weekly")
        );
        assert_eq!(event.trust, Trust::Untrusted);
        let own = source(Some(Trust::Trusted), json!({})).unwrap();
        assert_eq!(
            post(&own, &[], br#"{"title": "t"}"#).unwrap().trust,
            Trust::Trusted
        );
    }

    #[test]
    fn posts_without_a_key_are_named_by_their_signed_time_and_body() {
        let server = source(None, json!({})).unwrap();
        let body = br#"{"title": "disk full"}"#;
        let at = |time: &str| {
            post(&server, &[(TIMESTAMP_HEADER, time.into())], body)
                .unwrap()
                .key
        };
        assert_eq!(at("100"), at("100"));
        assert_ne!(at("100"), at("101"));
        assert!(at("100").starts_with("post:"));
        let unsigned = || post(&server, &[], body).unwrap().key;
        assert_ne!(unsigned(), unsigned());
    }

    #[test]
    fn a_source_with_its_own_secret_checks_the_signature_itself() {
        let own = source(None, json!({"secret_env": "REPORTS_SECRET"})).unwrap();
        assert_eq!(own.authentication(), PostAuthentication::Source);
        assert_eq!(
            source(None, json!({})).unwrap().authentication(),
            PostAuthentication::Server
        );
        let body = br#"{"title": "t"}"#;
        let now = unix_now();
        assert!(post(&own, &signed("own-secret", now, body), body).is_ok());
        for headers in [
            signed("server-secret", now, body),
            signed("own-secret", now - 1000, body),
            Vec::new(),
        ] {
            assert!(matches!(
                post(&own, &headers, body),
                Err(Rejection::Unauthorized(_))
            ));
        }
        assert!(source(None, json!({"secret_env": "MISSING"})).is_err());
        assert!(source(None, json!({"signature_tolerance_secs": 60})).is_err());
    }

    #[test]
    fn posts_with_missing_unknown_or_oversized_fields_are_refused() {
        for invalid in [
            json!({"title": " "}),
            json!({"title": "t", "workspace": "/"}),
            json!({"title": "t", "key": "k".repeat(201)}),
            json!({"title": "t", "author": "a".repeat(201)}),
            json!({"title": "t", "url": "u".repeat(2001)}),
            json!({"title": "t", "body": "b".repeat(100_001)}),
        ] {
            assert!(
                matches!(receive(invalid.clone()), Err(Rejection::Invalid(_))),
                "{invalid}"
            );
        }
        assert!(source(None, json!({"path": "/x"})).is_err());
    }
}
