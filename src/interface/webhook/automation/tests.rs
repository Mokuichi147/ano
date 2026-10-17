use super::{Automation, AutomationSettings};
use crate::{
    application::{
        agent::ModelTarget,
        automation::{
            AutomationRegistry, EventSource, Notification, Notifier, PostAuthentication,
            PostedRequest, Rejection,
        },
        ports::ResponsesApi,
        registry::ToolRegistry,
    },
    config::AppConfig,
    domain::automation::{AutomationEvent, Trust},
    harness::{
        notify::{AUTOMATION_INSTRUCTIONS, NOTIFY_INSTRUCTIONS},
        triage::Triage,
        Harness,
    },
    infrastructure::{
        automation::register_builtin_automation, automation_store::AutomationStore, mcp::McpPool,
    },
    interface::webhook::{
        jobs::JobState, post_event, signature::HmacSha256, unix_now, WebhookState,
    },
};
use anyhow::{bail, Result};
use async_trait::async_trait;
use axum::{
    body::{to_bytes, Bytes},
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    routing::post,
    Router,
};
use hmac::Mac;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{watch, RwLock, Semaphore},
    task::JoinSet,
};

/// Answers triage with `triage`, and the agent with a call of
/// `notify_user` followed by a final answer.
struct FakeApi {
    triage: String,
    agent_calls: Mutex<Vec<Value>>,
}

#[async_trait]
impl ResponsesApi for FakeApi {
    fn base_url(&self) -> &str {
        "http://model.test/v1"
    }

    async fn create_response(&self, payload: &Value) -> Result<Value> {
        let instructions = payload["instructions"].as_str().unwrap_or("");
        if instructions.starts_with("You triage events") {
            return Ok(json!({"output_text": self.triage}));
        }
        let mut calls = self.agent_calls.lock().unwrap();
        calls.push(payload.clone());
        Ok(if calls.len() == 1 {
            json!({"id": "r1", "status": "completed", "output": [{
                "type": "function_call", "call_id": "n1", "name": "notify_user",
                "arguments": json!({"title": "DAU drop", "message": "DAU fell 30%.", "priority": "high"}).to_string()
            }]})
        } else {
            json!({"id": "r2", "status": "completed", "output": [{
                "type": "message", "content": [{"type": "output_text", "text": "Reported the drop."}]
            }]})
        })
    }

    async fn compact_response(&self, _payload: &Value) -> Result<Value> {
        bail!("unused")
    }
}

type Received = Arc<Mutex<Vec<Value>>>;

async fn notifier_server() -> (String, Received) {
    let received: Received = Arc::default();
    let app = Router::new()
        .route(
            "/notify",
            post(|State(received): State<Received>, body: Bytes| async move {
                received
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap());
                "ok"
            }),
        )
        .with_state(Arc::clone(&received));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}/notify"), received)
}

struct Setup {
    state: Arc<WebhookState>,
    api: Arc<FakeApi>,
    received: Received,
    _dir: tempfile::TempDir,
}

fn builtin() -> AutomationRegistry {
    let mut registry = AutomationRegistry::new();
    register_builtin_automation(&mut registry);
    registry
}

async fn setup(rules: &str, triage: &str) -> Setup {
    setup_with(&builtin(), "", rules, triage).await
}

/// A server with the source `reports` (kind `webhook`, trusted, as a report
/// of the user's own service), the notifier
/// `phone` (kind `webhook`, recording into `received`), the settings in
/// `extra`, and `rules`, built with the kinds of `registry`.
async fn setup_with(
    registry: &AutomationRegistry,
    extra: &str,
    rules: &str,
    triage: &str,
) -> Setup {
    let (url, received) = notifier_server().await;
    let settings: AutomationSettings = toml::from_str(&format!(
        "[sources.reports]\nkind = 'webhook'\ntrust = 'trusted'\n\n[notifiers.phone]\nkind = 'webhook'\nurl = '{url}'\n\n{extra}\n\n{rules}"
    ))
    .unwrap();
    let mut config = AppConfig::default();
    config
        .environments
        .insert("coding".into(), Default::default());
    config.automation = settings.clone();
    config.validate().unwrap();

    let api = Arc::new(FakeApi {
        triage: triage.into(),
        agent_calls: Mutex::new(Vec::new()),
    });
    let dir = tempfile::tempdir().unwrap();
    let store = AutomationStore::open(&dir.path().join("state.json")).unwrap();
    let parts = settings.build(registry, &config.environments).unwrap();
    let triage = Triage::new(ModelTarget {
        client: api.clone(),
        model: "judge".into(),
        reasoning_effort: None,
        context_window: None,
    });
    let state = Arc::new(WebhookState {
        harness: Harness {
            registry: ToolRegistry::new(),
            history: None,
            skills: None,
        },
        config,
        client: api.clone(),
        connections: Default::default(),
        mcp: Arc::new(McpPool::new(Vec::new())),
        jobs: RwLock::new(HashMap::new()),
        job_slots: Arc::new(Semaphore::new(1)),
        shutdown: watch::channel(false).0,
        tasks: std::sync::Mutex::new(JoinSet::new()),
        secret: Some(b"secret".to_vec()),
        seen_signatures: std::sync::Mutex::new(HashMap::new()),
        automation: Some(Automation::new(settings, store, parts, triage)),
    });
    Setup {
        state,
        api,
        received,
        _dir: dir,
    }
}

fn signed(payload: &[u8]) -> HeaderMap {
    let timestamp = unix_now();
    let mut mac = HmacSha256::new_from_slice(b"secret").unwrap();
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(payload);
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-ano-signature",
        HeaderValue::from_str(&format!(
            "sha256={}",
            hex::encode(mac.finalize().into_bytes())
        ))
        .unwrap(),
    );
    headers.insert(
        "x-ano-timestamp",
        HeaderValue::from_str(&timestamp.to_string()).unwrap(),
    );
    headers
}

async fn post_signed(state: &Arc<WebhookState>, source: &str, event: Value) -> (StatusCode, Value) {
    let body = Bytes::from(serde_json::to_vec(&event).unwrap());
    let mut payload = format!("event:{source}:").into_bytes();
    payload.extend_from_slice(&body);
    let response = post_event(
        State(Arc::clone(state)),
        signed(&payload),
        Path(source.to_string()),
        body,
    )
    .await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn wait_for(received: &Received, count: usize) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let received = received.lock().unwrap().clone();
            if received.len() >= count {
                return received;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("notifications did not arrive")
}

const REPORT: &str = r#"{"kind": "weekly_report", "key": "2026-w42", "title": "Weekly active users", "body": "DAU: 700 (last week 1000)"}"#;

#[tokio::test]
async fn posted_reports_are_triaged_and_notified_once() {
    let setup = setup(
        "[[rules]]\nname = 'reports'\nnotify = ['phone']\nguidance = 'Tell me when users drop.'",
        r#"{"action": "notify", "priority": "high", "summary": "DAU fell from 1000 to 700.", "reason": "a sharp drop"}"#,
    )
    .await;
    let report: Value = serde_json::from_str(REPORT).unwrap();
    let (status, body) = post_signed(&setup.state, "reports", report.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["key"], "2026-w42");

    let received = wait_for(&setup.received, 1).await;
    assert_eq!(received[0]["title"], "Weekly active users");
    assert_eq!(received[0]["message"], "DAU fell from 1000 to 700.");
    assert_eq!(received[0]["priority"], "high");
    assert_eq!(received[0]["rule"], "reports");

    // The same key is handled once, even when the post is sent again.
    let mut again = report;
    again["title"] = json!("Weekly active users (resent)");
    let (status, body) = post_signed(&setup.state, "reports", again).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["duplicate"], true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(setup.received.lock().unwrap().len(), 1);
    assert!(setup.state.jobs.read().await.is_empty());
}

#[tokio::test]
async fn runs_tell_the_user_through_notify_user_and_on_completion() {
    let setup = setup(
        "[[rules]]\nname = 'weekly'\naction = 'run'\ntask = 'Read the report and notify me if the numbers dropped.'\nenvironment = 'coding'\nnotify = ['phone']\nnotify_on = ['completed']",
        "unused",
    )
    .await;
    let (status, _) = post_signed(
        &setup.state,
        "reports",
        serde_json::from_str(REPORT).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let received = wait_for(&setup.received, 2).await;
    assert_eq!(received[0]["title"], "DAU drop");
    assert_eq!(received[0]["priority"], "high");
    assert_eq!(
        received[1]["title"],
        "weekly: completed — Weekly active users"
    );
    assert_eq!(received[1]["message"], "Reported the drop.");

    let jobs = setup.state.jobs.read().await;
    let job = jobs.values().next().unwrap();
    assert_eq!(job.snapshot.status, JobState::Completed);
    let trigger = job.snapshot.trigger.as_ref().unwrap();
    assert_eq!(
        (trigger.rule.as_str(), trigger.key.as_str()),
        ("weekly", "2026-w42")
    );

    let calls = setup.api.agent_calls.lock().unwrap();
    let first = &calls[0];
    let instructions = first["instructions"].as_str().unwrap();
    assert!(instructions.contains(&format!("{AUTOMATION_INSTRUCTIONS} {NOTIFY_INSTRUCTIONS}")));
    assert!(first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "notify_user"));
    let input = first["input"].to_string();
    assert!(input.contains("Read the report and notify me"), "{input}");
    assert!(input.contains("DAU: 700"), "{input}");
}

#[tokio::test]
async fn events_need_their_own_signature_and_a_webhook_source() {
    let setup = setup("[[rules]]\nname = 'reports'\nnotify = ['phone']", "unused").await;
    // A signature over the body alone, as for a task, is not accepted.
    let body = Bytes::from_static(REPORT.as_bytes());
    let response = post_event(
        State(Arc::clone(&setup.state)),
        signed(&body),
        Path("reports".into()),
        body,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let (status, _) = post_signed(
        &setup.state,
        "unknown",
        serde_json::from_str(REPORT).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = post_signed(&setup.state, "reports", json!({"title": " "})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post_signed(
        &setup.state,
        "reports",
        json!({"title": "t", "workspace": "/"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    for oversized in [
        json!({"title": "t", "key": "k".repeat(201)}),
        json!({"title": "t", "author": "a".repeat(201)}),
        json!({"title": "t", "url": format!("https://example.com/{}", "u".repeat(2000))}),
    ] {
        let (status, _) = post_signed(&setup.state, "reports", oversized).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn a_full_queue_refuses_events_without_using_their_signature() {
    let setup = setup("[[rules]]\nname = 'reports'\nnotify = ['phone']", "unused").await;
    let automation = setup.state.automation.as_ref().unwrap();
    let held = Arc::clone(&automation.queued_events)
        .try_acquire_many_owned(super::MAX_QUEUED_EVENTS as u32)
        .unwrap();
    let (status, body) = post_signed(&setup.state, "reports", json!({"title": "t"})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(setup.state.seen_signatures.lock().unwrap().is_empty());
    drop(held);
    let (status, _) = post_signed(&setup.state, "reports", json!({"title": "t"})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

/// The first problem of the automation settings `text`, when they are
/// checked and built with the built-in kinds.
fn problem(text: &str) -> String {
    let mut config = AppConfig::default();
    config.environments.insert(
        "trusted".into(),
        toml::from_str("approval_mode = 'allow'").unwrap(),
    );
    config
        .environments
        .insert("safe".into(), Default::default());
    config.automation = toml::from_str(text).unwrap();
    let result = config.validate().and_then(|()| {
        config
            .automation
            .build(&builtin(), &config.environments)
            .map(|_| ())
    });
    format!("{:#}", result.unwrap_err())
}

#[test]
fn settings_refuse_rules_that_cannot_work_or_are_unsafe() {
    // A webhook source is untrusted unless set otherwise.
    let posted = "[sources.posted]\nkind = 'webhook'\n";
    assert!(problem(&format!(
        "{posted}[[rules]]\nname = 'fix'\ntask = 'Fix it'\nenvironment = 'trusted'"
    ))
    .contains("approval_mode is \"allow\""));
    assert!(problem(&format!(
        "{posted}[[rules]]\nname = 'a'\nnotify = ['nobody']"
    ))
    .contains("unknown notifier"));
    assert!(problem(&format!(
        "{posted}[[rules]]\nname = 'a'\nsources = ['other']"
    ))
    .contains("unknown source"));
    assert!(problem(&format!(
        "{posted}[[rules]]\nname = 'a'\ntask = 'x'\nenvironment = 'missing'"
    ))
    .contains("unknown environment"));
    assert!(problem("[sources.s]\nkind = 'rss'").contains("unknown source kind 'rss'"));
    assert!(problem("[sources.s]\nkind = 'github'").contains("unknown source kind 'github'"));
    assert!(
        problem("[notifiers.n]\nkind = 'mail'\n[sources.s]\nkind = 'webhook'")
            .contains("unknown notifier kind 'mail'")
    );
    // Each kind checks its own settings.
    assert!(problem("[sources.w]\nkind = 'webhook'\nignore_bots = true").contains("unknown field"));
    assert!(
        problem("[sources.w]\nkind = 'webhook'\nsignature_tolerance_secs = 5")
            .contains("needs secret_env")
    );
    assert!(problem("[sources.'a b']\nkind = 'webhook'").contains("source names"));

    // Trusted sources may run in an environment that approves every call.
    let mut config = AppConfig::default();
    config.environments.insert(
        "trusted".into(),
        toml::from_str("approval_mode = 'allow'").unwrap(),
    );
    config.automation = toml::from_str(
        "[sources.s]\nkind = 'webhook'\ntrust = 'trusted'\n[[rules]]\nname = 'weekly'\naction = 'run'\ntask = 'Check'\nenvironment = 'trusted'",
    )
    .unwrap();
    config.validate().unwrap();
    config
        .automation
        .build(&builtin(), &config.environments)
        .unwrap();
}

/// A source that takes posts in its own format, plain text, and checks them
/// itself, by a token in a header of its own, as services that sign their
/// requests their own way do.
struct PlainText;

#[async_trait]
impl EventSource for PlainText {
    fn trust(&self) -> Trust {
        Trust::Untrusted
    }

    fn describe(&self) -> String {
        "plain text".into()
    }

    fn authentication(&self) -> PostAuthentication {
        PostAuthentication::Source
    }

    fn receive(&self, request: &PostedRequest<'_>) -> Result<AutomationEvent, Rejection> {
        if request.header("X-Plain-Token") != Some("let-me-in") {
            return Err(Rejection::Unauthorized("wrong token".into()));
        }
        match std::str::from_utf8(request.body) {
            Ok(text) if !text.trim().is_empty() => Ok(AutomationEvent {
                source: request.source.into(),
                kind: "line".into(),
                key: text.trim().into(),
                title: text.trim().into(),
                body: String::new(),
                url: None,
                author: None,
                trust: Trust::Untrusted,
            }),
            _ => Err(Rejection::invalid("the body must be a line of text")),
        }
    }
}

/// A notifier of another kind: it keeps what it is sent.
struct Memory(Arc<Mutex<Vec<Notification>>>);

#[async_trait]
impl Notifier for Memory {
    async fn send(&self, notification: &Notification) -> Result<()> {
        self.0.lock().unwrap().push(notification.clone());
        Ok(())
    }
}

#[tokio::test]
async fn kinds_registered_by_the_embedder_work_like_the_built_in_ones() {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let mut registry = builtin();
    registry.register_source("plain", |spec| {
        if !spec
            .options
            .as_object()
            .is_some_and(|options| options.is_empty())
        {
            anyhow::bail!("plain takes no settings");
        }
        Ok(Arc::new(PlainText))
    });
    let memory = Arc::clone(&kept);
    registry.register_notifier("memory", move |_| Ok(Arc::new(Memory(Arc::clone(&memory)))));
    let setup = setup_with(
        &registry,
        "[sources.lines]\nkind = 'plain'\n[notifiers.kept]\nkind = 'memory'",
        "[[rules]]\nname = 'lines'\nsources = ['lines']\naction = 'notify'\nnotify = ['kept', 'phone']",
        "unused",
    )
    .await;

    // The source reads posts in its own format and checks them itself,
    // without ano's signature.
    let post_line = |token: &'static str, body: &'static [u8]| {
        let state = Arc::clone(&setup.state);
        async move {
            let mut headers = HeaderMap::new();
            headers.insert("x-plain-token", HeaderValue::from_static(token));
            post_event(
                State(state),
                headers,
                Path("lines".into()),
                Bytes::from_static(body),
            )
            .await
            .status()
        }
    };
    assert_eq!(
        post_line("guess", b"disk almost full").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(post_line("let-me-in", b" ").await, StatusCode::BAD_REQUEST);
    assert_eq!(
        post_line("let-me-in", b"disk almost full\n").await,
        StatusCode::ACCEPTED
    );
    // The built-in notifier and the embedder's both hear of it.
    let received = wait_for(&setup.received, 1).await;
    assert_eq!(received[0]["title"], "disk almost full");
    let titles: Vec<String> = kept
        .lock()
        .unwrap()
        .iter()
        .map(|notification| notification.title.clone())
        .collect();
    assert_eq!(titles, ["disk almost full"]);
    // The same line again is the same event.
    assert_eq!(
        post_line("let-me-in", b"disk almost full\n").await,
        StatusCode::OK
    );
    // Unknown sources are told apart only for requests with ano's
    // signature.
    let (status, _) = post_signed(&setup.state, "missing", json!({"title": "t"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let unsigned = post_event(
        State(Arc::clone(&setup.state)),
        HeaderMap::new(),
        Path("missing".into()),
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(unsigned.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn the_example_in_config_example_toml_is_valid() {
    let example = include_str!("../../../../config.example.toml");
    let start = example.find("# [automation]\n").unwrap();
    let end = start + example[start..].find("\n\n").unwrap();
    let uncommented: String = example[start..end]
        .lines()
        .filter_map(|line| line.strip_prefix("# ").or(line.strip_prefix("#")))
        .filter(|line| !line.starts_with('#'))
        .map(|line| format!("{line}\n"))
        .collect();
    let config = AppConfig::parse(&format!(
        "{uncommented}\n[environments.coding]\napproval_mode = 'auto'\n"
    ))
    .unwrap();
    // Building reads the variables the notifiers and sources name.
    std::env::set_var(
        "DISCORD_WEBHOOK_URL",
        "https://discord.com/api/webhooks/1/x",
    );
    std::env::set_var("ANALYTICS_SECRET", "secret");
    std::env::set_var("CRON_SECRET", "secret");
    config
        .automation
        .build(&builtin(), &config.environments)
        .unwrap();
    assert_eq!(config.automation.sources.len(), 2);
    assert_eq!(config.automation.rules.len(), 2);
    assert_eq!(config.automation.notifiers.len(), 2);
}
