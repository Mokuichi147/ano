//! The built-in kinds of automation: the source `webhook`, and the
//! notifiers `ntfy`, `discord`, `slack`, and `webhook`.
//! Each reads and checks its own settings, and is registered by its kind
//! like the kinds a harness or library user adds.

pub mod notify;
pub mod webhook;

use crate::application::automation::{AutomationRegistry, KindSpec};
use anyhow::{bail, Context, Result};
use notify::{HttpNotifier, HttpNotifierKind, HttpNotifierSettings};
use reqwest::{Response, Url};
use serde::de::DeserializeOwned;
use std::net::IpAddr;
use std::sync::Arc;

/// Register the built-in sources and notifiers in `registry`.
pub fn register_builtin_automation(registry: &mut AutomationRegistry) {
    registry.register_source("webhook", |spec| {
        Ok(Arc::new(webhook::WebhookSource::new(spec)?))
    });
    for kind in HttpNotifierKind::ALL {
        registry.register_notifier(kind.name(), move |spec| {
            let settings: HttpNotifierSettings = options(spec)?;
            Ok(Arc::new(HttpNotifier::from_settings(
                kind,
                &settings,
                |name| std::env::var(name).ok(),
            )?))
        });
    }
}

/// The settings of `spec` as the kind's own settings type, which should
/// deny unknown fields so that a mistyped setting is an error.
pub fn options<T: DeserializeOwned>(spec: &KindSpec<'_>) -> Result<T> {
    serde_json::from_value(spec.options.clone()).context("invalid settings")
}

/// `url` when it is HTTPS, or HTTP to this machine: the kinds send tokens,
/// secrets, and what the user is told, which plain HTTP would expose on the
/// network. Errors leave the URL out, since it may be a secret.
pub fn secure_url(url: &str, what: &str) -> Result<Url> {
    let Some(parsed) = Url::parse(url).ok().filter(|url| url.host().is_some()) else {
        bail!("{what} must be an https:// URL");
    };
    let host = parsed
        .host_str()
        .unwrap_or("")
        .trim_start_matches('[')
        .trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if loopback => Ok(parsed),
        "http" => bail!("{what} must use https://; plain http:// is only for this machine"),
        _ => bail!("{what} must be an https:// URL"),
    }
}

/// At most `limit` bytes of the body of `response`, so a server cannot
/// make the kind hold an unbounded answer. Returns whether it was cut.
pub async fn read_limited(mut response: Response, limit: usize) -> Result<(Vec<u8>, bool)> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| error.without_url())
        .context("failed to read the response")?
    {
        let room = limit - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

/// The start of the body of a failed `response`, for its error message.
pub async fn error_excerpt(response: Response) -> String {
    let body = read_limited(response, 4096)
        .await
        .map(|(body, _)| body)
        .unwrap_or_default();
    String::from_utf8_lossy(&body)
        .trim()
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_go_over_https_or_stay_on_this_machine() {
        for allowed in [
            "https://ntfy.sh/topic",
            "http://127.0.0.1:8080/hook",
            "http://localhost/hook",
            "http://[::1]:9/hook",
        ] {
            assert!(secure_url(allowed, "the URL").is_ok(), "{allowed}");
        }
        for refused in [
            "http://example.com/hook",
            "http://10.0.0.5/hook",
            "ftp://example.com/x",
            "https://",
        ] {
            let error = secure_url(refused, "the URL").unwrap_err().to_string();
            assert!(error.contains("https://"), "{refused}: {error}");
            assert!(!error.contains("example.com"), "{error}");
        }
    }
}
