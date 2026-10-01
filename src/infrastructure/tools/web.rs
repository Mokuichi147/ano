//! `web_fetch`: read a web page as Markdown text.
//!
//! Only available when the environment sets `allow_web`, and every call goes
//! through the run's approval handler first, because a URL can carry data out
//! of the workspace. Only public addresses are fetched: the host is resolved
//! once, every address is checked, and the connection is pinned to them, so a
//! page cannot reach the local network or cloud metadata endpoints, also not
//! through a redirect or a changed DNS answer.

use super::args::{optional_bool, optional_integer};
use crate::{
    application::registry::ToolRegistry,
    domain::tool::{ToolContext, ToolDefinition},
    harness::names::WEB_FETCH_NAME,
};
use anyhow::{bail, Context, Result};
use futures::StreamExt;
use reqwest::{redirect::Policy, Url};
use serde_json::{json, Value};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::LazyLock,
    time::Duration,
};

const MAX_DOWNLOAD_BYTES: usize = 5 * 1024 * 1024;
const DEFAULT_PAGE_BYTES: u64 = 32 * 1024;
const MAX_PAGE_BYTES: u64 = 256 * 1024;
const MAX_REDIRECTS: usize = 5;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn register(registry: &ToolRegistry) -> Result<()> {
    let mut definition = ToolDefinition::new(
        WEB_FETCH_NAME,
        "Fetch a public web page over HTTP(S) and return its content, with HTML converted to Markdown. Use it to read documentation, issues, or references the task needs. Each fetch needs approval and may be denied; never put secrets or workspace data into the URL. Long pages are returned in pages of max_bytes (32 KiB by default); continue from next_offset.",
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "minLength": 1, "description": "http:// or https:// URL"},
                "offset": {"type": "integer", "minimum": 0, "description": "Byte offset into the converted content from a previous next_offset; defaults to 0"},
                "max_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_PAGE_BYTES, "description": "Content bytes to return; defaults to 32768"},
                "raw": {"type": "boolean", "description": "Return HTML as it is instead of converting it to Markdown; defaults to false"}
            },
            "required": ["url"],
            "additionalProperties": false
        }),
    )
    .with_approval()
    .available_when(|context| context.allow_web);
    definition.strict = false;
    registry.register_contextual(definition, |arguments, context| async move {
        web_fetch(arguments, &context).await
    })
}

async fn web_fetch(arguments: Value, context: &ToolContext) -> Result<Value> {
    if !context.allow_web {
        bail!("web access is not enabled for this environment (allow_web)");
    }
    let url = arguments["url"]
        .as_str()
        .context("web_fetch.url must be a string")?;
    let offset = optional_integer(&arguments, "offset", 0, 0, u64::MAX)?;
    let max_bytes = optional_integer(
        &arguments,
        "max_bytes",
        DEFAULT_PAGE_BYTES,
        1,
        MAX_PAGE_BYTES,
    )?;
    let raw = optional_bool(&arguments, "raw")?;
    let page = fetch(url, AddressPolicy::PublicOnly).await?;
    Ok(page.to_output(raw, offset as usize, max_bytes as usize))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AddressPolicy {
    PublicOnly,
    /// Tests fetch from a server on the loopback interface.
    #[cfg(test)]
    Any,
}

struct Page {
    url: String,
    status: u16,
    content_type: String,
    body: String,
    /// The download limit cut the body short.
    body_truncated: bool,
}

async fn fetch(url: &str, policy: AddressPolicy) -> Result<Page> {
    let mut url = parse_url(url)?;
    for _ in 0..=MAX_REDIRECTS {
        let addresses = resolve(&url, policy).await?;
        let host = url.host_str().context("URL has no host")?.to_string();
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .timeout(FETCH_TIMEOUT)
            .resolve_to_addrs(&host, &addresses)
            .user_agent(concat!("ano/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build HTTP client")?;
        let response = client
            .get(url.clone())
            .header(
                reqwest::header::ACCEPT,
                "text/html, text/markdown, text/plain, application/json;q=0.9, */*;q=0.5",
            )
            .send()
            .await
            .with_context(|| format!("failed to fetch {url}"))?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .with_context(|| format!("{url} redirected without a location"))?;
            url = parse_url(url.join(location)?.as_str())?;
            continue;
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !is_text(&content_type) {
            bail!("{url} returned unsupported content type '{content_type}'; only text, HTML, JSON, and XML can be read");
        }
        let mut body = Vec::new();
        let mut body_truncated = false;
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.with_context(|| format!("failed to read {url}"))?;
            let room = MAX_DOWNLOAD_BYTES - body.len();
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                body_truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        return Ok(Page {
            url: url.to_string(),
            status: status.as_u16(),
            content_type,
            body: String::from_utf8_lossy(&body).into_owned(),
            body_truncated,
        });
    }
    bail!("stopped after {MAX_REDIRECTS} redirects")
}

fn parse_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw.trim()).with_context(|| format!("invalid URL '{raw}'"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("only http and https URLs can be fetched");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URLs with credentials cannot be fetched");
    }
    if url.host().is_none() {
        bail!("URL has no host");
    }
    Ok(url)
}

/// Resolve the host of `url` and check every address against `policy`.
async fn resolve(url: &Url, policy: AddressPolicy) -> Result<Vec<SocketAddr>> {
    let host = url.host_str().context("URL has no host")?;
    let port = url.port_or_known_default().unwrap_or(80);
    // An IPv6 literal keeps its brackets in host_str.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("failed to resolve {host}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        bail!("{host} did not resolve to any address");
    }
    if policy == AddressPolicy::PublicOnly {
        if let Some(address) = addresses.iter().find(|address| !is_public(address.ip())) {
            bail!(
                "{host} resolves to {}, which is not a public address; web_fetch reads only public web pages",
                address.ip()
            );
        }
    }
    Ok(addresses)
}

fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return is_public_v4(mapped);
            }
            let first = address.segments()[0];
            !(address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local
                || (first & 0xffc0) == 0xfe80 // link local
                || (first == 0x2001 && address.segments()[1] == 0x0db8) // documentation
                || first == 0x0064) // NAT64 (64:ff9b::/96 and 64:ff9b:1::/48), which reaches IPv4 hosts
        }
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, ..] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_broadcast()
        || address.is_multicast()
        || address.is_documentation()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // shared address space
        || (a == 192 && b == 0 && address.octets()[2] == 0) // protocol assignments
        || (a == 198 && (18..20).contains(&b)) // benchmarking
        || a >= 240)
}

fn is_text(content_type: &str) -> bool {
    content_type.is_empty()
        || content_type.starts_with("text/")
        || ["json", "xml", "javascript", "yaml", "toml"]
            .iter()
            .any(|kind| content_type.contains(kind))
}

impl Page {
    fn is_html(&self) -> bool {
        self.content_type.contains("html")
            || (self.content_type.is_empty()
                && self.body.trim_start().get(..15).is_some_and(|start| {
                    start.eq_ignore_ascii_case("<!doctype html>") || start.starts_with("<html")
                }))
    }

    fn to_output(&self, raw: bool, offset: usize, max_bytes: usize) -> Value {
        let html = self.is_html();
        let (title, content) = if html && !raw {
            (html_title(&self.body), html_to_markdown(&self.body))
        } else {
            (None, self.body.clone())
        };
        let start = floor_boundary(&content, offset.min(content.len()));
        let end = floor_boundary(&content, start.saturating_add(max_bytes).min(content.len()));
        let mut output = json!({
            "url": self.url,
            "status": self.status,
            "content_type": self.content_type,
            "format": if html && !raw { "markdown" } else { "text" },
            "content": &content[start..end],
            "offset": start,
            "total_bytes": content.len(),
        });
        if let Some(title) = title {
            output["title"] = json!(title);
        }
        if end < content.len() {
            output["next_offset"] = json!(end);
        }
        if self.body_truncated {
            output["download_truncated"] = json!(true);
        }
        output
    }
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn html_title(html: &str) -> Option<String> {
    static TITLE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
    let title = TITLE.captures(html)?.get(1)?.as_str();
    let title = htmd::convert(title).unwrap_or_else(|_| title.to_string());
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then_some(title)
}

fn html_to_markdown(html: &str) -> String {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec![
            "head", "script", "style", "noscript", "svg", "iframe", "template", "nav", "footer",
            "form", "button",
        ])
        .build();
    let markdown = converter.convert(html).unwrap_or_default();
    // Collapse the blank lines left by removed elements.
    let mut output = String::with_capacity(markdown.len());
    let mut blank = 0;
    for line in markdown.lines() {
        if line.trim().is_empty() {
            blank += 1;
            continue;
        }
        if !output.is_empty() {
            output.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        output.push_str(line.trim_end());
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        http::header,
        response::{IntoResponse, Redirect},
        routing::get,
        Router,
    };

    #[test]
    fn only_public_addresses_are_allowed() {
        for address in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.11.51",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!is_public(address.parse().unwrap()), "{address}");
        }
        for address in ["93.184.215.14", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public(address.parse().unwrap()), "{address}");
        }
    }

    #[tokio::test]
    async fn rejects_local_urls_and_other_schemes() {
        for url in [
            "http://127.0.0.1:8080/",
            "http://localhost/",
            "http://[::1]/",
            "file:///etc/passwd",
            "ftp://example.com/",
            "http://user:pass@example.com/",
            "not a url",
        ] {
            assert!(
                fetch(url, AddressPolicy::PublicOnly).await.is_err(),
                "{url}"
            );
        }
    }

    #[tokio::test]
    async fn requires_allow_web() {
        let error = web_fetch(
            json!({"url": "https://example.com/"}),
            &ToolContext::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("allow_web"));
    }

    #[tokio::test]
    async fn converts_html_follows_redirects_and_pages_content() {
        let app = Router::new()
            .route("/old", get(|| async { Redirect::temporary("/doc") }))
            .route(
                "/doc",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                        "<!doctype html><html><head><title> ano &amp; docs </title><style>p{}</style></head><body><nav>menu</nav><h1>見出し</h1><p>本文の<b>強調</b></p><script>alert(1)</script><ul><li>一</li><li>二</li></ul></body></html>",
                    )
                        .into_response()
                }),
            )
            .route(
                "/image",
                get(|| async { ([(header::CONTENT_TYPE, "image/png")], vec![0u8; 4]) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let page = fetch(&format!("{base}/old"), AddressPolicy::Any)
            .await
            .unwrap();
        assert!(page.url.ends_with("/doc"));
        let output = page.to_output(false, 0, 1000);
        assert_eq!(output["title"], "ano & docs");
        assert_eq!(output["format"], "markdown");
        let content = output["content"].as_str().unwrap();
        assert!(content.contains("# 見出し"), "{content}");
        assert!(content.contains("本文の**強調**"), "{content}");
        assert!(
            content.contains("*   一") || content.contains("- 一") || content.contains("* 一"),
            "{content}"
        );
        for hidden in ["alert", "menu", "p{}"] {
            assert!(!content.contains(hidden), "{content}");
        }
        assert!(output.get("next_offset").is_none());

        // Pages split at character boundaries and can be reassembled.
        let whole = content.to_string();
        let mut offset = 0;
        let mut pieces = String::new();
        loop {
            let output = page.to_output(false, offset, 5);
            pieces.push_str(output["content"].as_str().unwrap());
            match output["next_offset"].as_u64() {
                Some(next) if next as usize > offset => offset = next as usize,
                Some(_) => offset += 1,
                None => break,
            }
        }
        assert_eq!(pieces, whole);

        let raw = page.to_output(true, 0, 100_000);
        assert!(raw["content"].as_str().unwrap().contains("<script>"));

        let error = fetch(&format!("{base}/image"), AddressPolicy::Any)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("unsupported content type"));
        server.abort();
    }
}
