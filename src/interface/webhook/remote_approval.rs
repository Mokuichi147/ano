//! Approval of automation jobs from a phone: a call that needs the user's
//! approval waits, while the rule's notifiers send a link to a page where
//! the user allows or denies it. No answer before the time limit denies it.
//!
//! The link carries a random token for that one request, so whoever has the
//! notification can answer it, and only it, once.

use super::{automation::send_all, WebhookState};
use crate::{
    application::{
        automation::{NamedNotifier, Notification},
        clock::unix_now,
        ports::{ApprovalHandler, ApprovalSource, McpApprovalRequest},
    },
    domain::{automation::Priority, text::cut},
    interface::{escape_html as escape, shorten},
};
use anyhow::{bail, Result};
use async_trait::async_trait;
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    Form,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;
use uuid::Uuid;

/// Longer arguments are cut on the page and in the notification.
const MAX_PAGE_ARGUMENT_CHARS: usize = 4000;
const MAX_NOTIFIED_ARGUMENT_CHARS: usize = 600;
/// The page needs nothing from elsewhere and runs no script.
const PAGE_POLICY: &str =
    "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

/// `[automation.approvals]`: where the server is reached from the phone, and
/// how long a request waits.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalSettings {
    /// The URL the links start with, as the phone reaches this server, such
    /// as `https://ano.example.com` behind a reverse proxy.
    pub public_url: String,
    /// How long a request waits for the answer before it is denied.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    600
}

impl ApprovalSettings {
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(self.public_url.trim()).map_err(|_| {
            anyhow::anyhow!("automation.approvals.public_url must be an http(s) URL")
        })?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            bail!("automation.approvals.public_url must be an http(s) URL");
        }
        if url.query().is_some() || url.fragment().is_some() {
            bail!("automation.approvals.public_url must not have a query or fragment");
        }
        if !(10..=86_400).contains(&self.timeout_secs) {
            bail!("automation.approvals.timeout_secs must be 10 to 86400");
        }
        Ok(())
    }

    fn base(&self) -> String {
        self.public_url.trim().trim_end_matches('/').to_string()
    }
}

/// A request that waits for its answer.
struct Pending {
    token: String,
    reply: oneshot::Sender<bool>,
    rule: String,
    target: String,
    arguments: String,
    expires_at: u64,
}

/// The requests of a server that wait for the user's answer.
pub(super) struct RemoteApprovals {
    settings: ApprovalSettings,
    pending: Mutex<HashMap<String, Pending>>,
}

impl RemoteApprovals {
    pub(super) fn new(settings: ApprovalSettings) -> Self {
        Self {
            settings,
            pending: Mutex::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The handler of a job of `rule`, which asks through `notifiers`.
    pub(super) fn handler(
        self: &Arc<Self>,
        rule: &str,
        notifiers: Vec<NamedNotifier>,
    ) -> NotifyApproval {
        NotifyApproval {
            approvals: Arc::clone(self),
            rule: rule.to_string(),
            notifiers,
        }
    }

    /// The request `id`, when `token` is its token and it still waits.
    fn find(&self, id: &str, token: &str) -> Option<(String, String, String, u64)> {
        let pending = self.lock();
        let request = pending.get(id)?;
        same(&request.token, token).then(|| {
            (
                request.rule.clone(),
                request.target.clone(),
                request.arguments.clone(),
                request.expires_at,
            )
        })
    }

    /// Answer the request `id`. False when `token` is not its token or it no
    /// longer waits.
    fn answer(&self, id: &str, token: &str, approved: bool) -> bool {
        let mut pending = self.lock();
        if !pending
            .get(id)
            .is_some_and(|request| same(&request.token, token))
        {
            return false;
        }
        pending
            .remove(id)
            .is_some_and(|request| request.reply.send(approved).is_ok())
    }
}

/// Compare tokens in time that does not depend on where they differ.
fn same(expected: &str, given: &str) -> bool {
    expected.len() == given.len()
        && expected
            .bytes()
            .zip(given.bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// Asks the user of a job by notification and waits for the answer.
pub(super) struct NotifyApproval {
    approvals: Arc<RemoteApprovals>,
    rule: String,
    notifiers: Vec<NamedNotifier>,
}

/// Withdraws a request whose wait ended without an answer.
struct Withdraw<'a>(&'a RemoteApprovals, &'a str);

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        self.0.lock().remove(self.1);
    }
}

#[async_trait]
impl ApprovalHandler for NotifyApproval {
    async fn approve(&self, request: McpApprovalRequest) -> Result<bool> {
        let id = Uuid::new_v4().simple().to_string();
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let mut arguments = request.arguments.clone();
        shorten(&mut arguments, MAX_PAGE_ARGUMENT_CHARS);
        let arguments = serde_json::to_string_pretty(&arguments).unwrap_or_default();
        let target = match request.source {
            ApprovalSource::Mcp => format!("MCP {}", request.target()),
            ApprovalSource::LocalTool => request.target(),
        };
        let timeout = self.approvals.settings.timeout_secs;
        let (reply, answer) = oneshot::channel();
        self.approvals.lock().insert(
            id.clone(),
            Pending {
                token: token.clone(),
                reply,
                rule: self.rule.clone(),
                target: target.clone(),
                arguments: arguments.clone(),
                expires_at: unix_now() + timeout,
            },
        );
        let _withdraw = Withdraw(&self.approvals, &id);
        let link = format!(
            "{}/approvals/{id}?t={token}",
            self.approvals.settings.base()
        );
        send_all(
            &self.notifiers,
            &Notification {
                title: format!("承認が必要です — {}", self.rule),
                message: format!(
                    "`{target}` を実行してよいか確認してください（{} 分以内。過ぎると拒否します）。\n\n```\n{}\n```",
                    timeout.div_ceil(60),
                    cut(&arguments, MAX_NOTIFIED_ARGUMENT_CHARS)
                ),
                priority: Priority::High,
                url: Some(link),
                rule: Some(self.rule.clone()),
            },
        )
        .await;
        let approved = match tokio::time::timeout(Duration::from_secs(timeout), answer).await {
            Ok(answer) => answer.unwrap_or(false),
            Err(_) => {
                println!(
                    "automation: rule '{}': approval of {target} timed out and was denied",
                    self.rule
                );
                false
            }
        };
        Ok(approved)
    }
}

#[derive(Deserialize)]
pub(super) struct TokenQuery {
    t: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct AnswerForm {
    t: String,
    decision: String,
}

/// The page of a waiting request: what it would run, and two buttons.
pub(super) async fn approval_page(
    State(state): State<Arc<WebhookState>>,
    Path(id): Path<String>,
    Query(query): Query<TokenQuery>,
) -> Response {
    let found = state
        .approvals
        .as_ref()
        .zip(query.t.as_deref())
        .and_then(|(approvals, token)| approvals.find(&id, token).map(|found| (found, token)));
    let Some(((rule, target, arguments, expires_at), token)) = found else {
        return page(
            StatusCode::NOT_FOUND,
            "この承認は見つかりません",
            "<p>すでに答えたか、期限が過ぎたか、作業が終わりました。</p>",
        );
    };
    let left = expires_at.saturating_sub(unix_now()).div_ceil(60);
    page(
        StatusCode::OK,
        "承認が必要です",
        &format!(
            "<p>ルール <b>{}</b> の作業が、次の操作の承認を求めています（あと約 {left} 分。過ぎると拒否します）。</p>\
             <p class=\"target\">{}</p><pre>{}</pre>\
             <form method=\"post\"><input type=\"hidden\" name=\"t\" value=\"{}\">\
             <button name=\"decision\" value=\"allow\" class=\"allow\">許可</button>\
             <button name=\"decision\" value=\"deny\">拒否</button></form>",
            escape(&rule),
            escape(&target),
            escape(&arguments),
            escape(token),
        ),
    )
}

/// The answer from the page.
pub(super) async fn answer_approval(
    State(state): State<Arc<WebhookState>>,
    Path(id): Path<String>,
    Form(form): Form<AnswerForm>,
) -> Response {
    let approved = match form.decision.as_str() {
        "allow" => true,
        "deny" => false,
        _ => return page(StatusCode::BAD_REQUEST, "答えが読めません", ""),
    };
    let answered = state
        .approvals
        .as_ref()
        .is_some_and(|approvals| approvals.answer(&id, &form.t, approved));
    if !answered {
        return page(
            StatusCode::NOT_FOUND,
            "この承認は見つかりません",
            "<p>すでに答えたか、期限が過ぎたか、作業が終わりました。</p>",
        );
    }
    page(
        StatusCode::OK,
        if approved {
            "許可しました"
        } else {
            "拒否しました"
        },
        "<p>作業はこの答えで続きます。このページは閉じてかまいません。</p>",
    )
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"ja\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"referrer\" content=\"no-referrer\"><title>ano — {title}</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:640px;margin:0 auto;padding:16px;line-height:1.6;color-scheme:light dark}}\
         pre{{white-space:pre-wrap;word-break:break-all;background:rgba(127,127,127,.12);padding:12px;border-radius:8px;font-size:14px}}\
         .target{{font-weight:bold;word-break:break-all}}form{{display:flex;gap:12px;margin-top:16px}}\
         button{{flex:1;font-size:18px;padding:14px;border-radius:10px;border:1px solid #888}}.allow{{background:#2b6;color:#fff;border-color:#2b6}}</style>\
         <h1>{title}</h1>{body}</html>"
    );
    (
        status,
        [
            (header::CONTENT_SECURITY_POLICY, PAGE_POLICY),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(html),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_need_a_url_and_a_sensible_timeout() {
        let settings = |url: &str, timeout_secs| ApprovalSettings {
            public_url: url.into(),
            timeout_secs,
        };
        settings("https://ano.example.com/", 600)
            .validate()
            .unwrap();
        settings("http://192.168.1.5:8080", 60).validate().unwrap();
        for (url, timeout) in [
            ("ano.example.com", 600),
            ("ftp://example.com", 600),
            ("https://example.com/?x=1", 600),
            ("https://example.com", 5),
        ] {
            assert!(settings(url, timeout).validate().is_err(), "{url}");
        }
        assert_eq!(
            settings("https://a.example/ano/", 600).base(),
            "https://a.example/ano"
        );
    }

    #[test]
    fn tokens_are_compared_whole() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
        assert_eq!(
            escape("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }
}
