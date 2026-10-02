//! Who may use the web UI: a browser on this machine, or one that presents
//! the token printed at startup (held in a cookie), unless authentication is
//! turned off. Other sites cannot use a browser to act on the server: a
//! request on this machine must name this machine as its host (against DNS
//! rebinding), and requests that change something must come from the page's
//! own origin.

use axum::http::{header, uri::Authority, HeaderMap, Method};
use std::net::{IpAddr, SocketAddr};

pub(super) struct Access {
    /// The token other machines present; `None` admits everyone.
    token: Option<String>,
    /// The cookie that holds the token; named by port, so servers on other
    /// ports of the same host keep their own.
    cookie: String,
}

impl Access {
    pub(super) fn new(token: Option<String>, port: u16) -> Self {
        Self {
            token,
            cookie: format!("ano_web_{port}"),
        }
    }

    pub(super) fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Whether a request from `peer` with `headers` may use the UI.
    pub(super) fn admits(&self, peer: SocketAddr, headers: &HeaderMap) -> bool {
        self.token.is_none() || from_this_machine(peer, headers) || self.has_token(headers)
    }

    /// Whether `given` is the token.
    pub(super) fn is_token(&self, given: &str) -> bool {
        self.token
            .as_deref()
            .is_some_and(|token| same_token(given, token))
    }

    /// The `Set-Cookie` value that stores the token in the browser.
    pub(super) fn cookie(&self) -> Option<String> {
        let token = self.token.as_deref()?;
        Some(format!(
            "{}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000",
            self.cookie
        ))
    }

    fn has_token(&self, headers: &HeaderMap) -> bool {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|cookies| cookies.split(';'))
            .filter_map(|cookie| cookie.trim().split_once('='))
            .any(|(name, value)| name == self.cookie && self.is_token(value))
    }
}

/// The request comes from this machine and names it as the host. A page of
/// another site that resolves its name to 127.0.0.1 (DNS rebinding) sends
/// its own name.
fn from_this_machine(peer: SocketAddr, headers: &HeaderMap) -> bool {
    peer.ip().is_loopback()
        && host(headers).is_some_and(|host| {
            let host = host.to_ascii_lowercase();
            host == "localhost"
                || host.ends_with(".localhost")
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

/// The host of the `Host` header, without the port.
fn host(headers: &HeaderMap) -> Option<String> {
    let authority: Authority = headers.get(header::HOST)?.to_str().ok()?.parse().ok()?;
    Some(authority.host().to_string())
}

/// Whether a request may change something: reading is always fine, and a
/// browser names the origin of other requests, which must be the page's.
/// Requests without `Origin` do not come from a page of another site.
pub(super) fn same_origin(method: &Method, headers: &HeaderMap) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return true;
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let origin = origin.to_str().unwrap_or_default();
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .unwrap_or_default();
    ["http://", "https://"]
        .iter()
        .filter_map(|scheme| origin.strip_prefix(scheme))
        .any(|authority| !host.is_empty() && authority.eq_ignore_ascii_case(host))
}

/// Compare without stopping at the first difference.
fn same_token(given: &str, token: &str) -> bool {
    given.len() == token.len()
        && given
            .bytes()
            .zip(token.bytes())
            .fold(0, |difference, (a, b)| difference | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn this_machine_needs_no_token_but_others_and_other_sites_do() {
        let access = Access::new(Some("secret".into()), 8787);
        let local: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let lan: SocketAddr = "192.168.1.20:50000".parse().unwrap();

        for host in [
            "127.0.0.1:8787",
            "localhost:8787",
            "[::1]:8787",
            "ano.localhost",
        ] {
            assert!(access.admits(local, &headers(&[("host", host)])), "{host}");
        }
        // DNS rebinding: another site's name that resolves to this machine.
        assert!(!access.admits(local, &headers(&[("host", "attacker.example:8787")])));
        assert!(!access.admits(local, &headers(&[])));
        // Another machine, even naming 127.0.0.1.
        assert!(!access.admits(lan, &headers(&[("host", "127.0.0.1:8787")])));
        let with_cookie = headers(&[
            ("host", "192.168.1.5:8787"),
            ("cookie", "other=1; ano_web_8787=secret"),
        ]);
        assert!(access.admits(lan, &with_cookie));
        let wrong = headers(&[
            ("host", "192.168.1.5:8787"),
            ("cookie", "ano_web_8787=secreT"),
        ]);
        assert!(!access.admits(lan, &wrong));
        let other_port = headers(&[
            ("host", "192.168.1.5:8787"),
            ("cookie", "ano_web_9000=secret"),
        ]);
        assert!(!access.admits(lan, &other_port));

        let open = Access::new(None, 8787);
        assert!(open.admits(lan, &headers(&[("host", "192.168.1.5:8787")])));
        assert!(open.cookie().is_none());
        assert!(!open.is_token(""));
    }

    #[test]
    fn only_the_pages_own_origin_may_change_something() {
        let post = Method::POST;
        let page = headers(&[
            ("host", "127.0.0.1:8787"),
            ("origin", "http://127.0.0.1:8787"),
        ]);
        assert!(same_origin(&post, &page));
        assert!(same_origin(&Method::DELETE, &page));
        let other = headers(&[
            ("host", "127.0.0.1:8787"),
            ("origin", "https://attacker.example"),
        ]);
        assert!(!same_origin(&post, &other));
        assert!(!same_origin(
            &post,
            &headers(&[("host", "127.0.0.1:8787"), ("origin", "null")])
        ));
        assert!(same_origin(&Method::GET, &other));
        // Clients other than browsers send no origin.
        assert!(same_origin(&post, &headers(&[("host", "127.0.0.1:8787")])));
    }
}
