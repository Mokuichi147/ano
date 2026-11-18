//! Automation of `ano serve`: events posted to the configured sources are
//! matched to rules, triaged, and then ignored, notified, or run as jobs
//! whose results are notified. Sources and notifiers are whatever kinds the
//! `AutomationRegistry` has; this module only uses them.

mod settings;
#[cfg(test)]
mod tests;

pub use settings::{AutomationParts, AutomationSettings, NotifierSettings, SourceSettings};

use super::{admit_job, JobSpec, WebhookState};
use crate::{
    application::{
        agent::ModelTarget,
        automation::{
            unix_now, Admission, AutomationRegistry, EventSource, NamedNotifier, Notification,
        },
        input::InputPart,
    },
    config::AppConfig,
    domain::{
        automation::{
            event_run_request, matching_rule, AutomationEvent, Priority, TriageAction, TriggerRule,
        },
        text::cut,
    },
    harness::triage::Triage,
    infrastructure::{
        automation::schedule::local_time,
        automation_store::{default_state_file, AutomationStore},
    },
};
use anyhow::{Context, Result};
use futures::future::join_all;
use serde::Serialize;
use std::time::Duration;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Job results longer than this are cut in notifications.
const MAX_RESULT_CHARS: usize = 1500;
/// Posted events waiting for or in triage; further posts get 503.
pub(super) const MAX_QUEUED_EVENTS: usize = 64;
/// Events triaged and acted on at the same time.
const MAX_CONCURRENT_EVENTS: usize = 4;

/// The automation of one server.
pub(in crate::interface::webhook) struct Automation {
    pub settings: AutomationSettings,
    pub store: Arc<AutomationStore>,
    sources: BTreeMap<String, Arc<dyn EventSource>>,
    notifiers: BTreeMap<String, NamedNotifier>,
    triage: Triage,
    /// Posted events that are queued or being handled.
    pub queued_events: Arc<Semaphore>,
    /// Slots for events being triaged and acted on.
    handling: Arc<Semaphore>,
}

/// What a job that automation started carries.
pub(in crate::interface::webhook) struct JobTrigger {
    pub rule: TriggerRule,
    pub event: AutomationEvent,
    pub notifiers: Vec<NamedNotifier>,
}

/// The rule and event of a job, in its status.
#[derive(Debug, Clone, Serialize)]
pub struct TriggerInfo {
    pub rule: String,
    pub source: String,
    pub kind: String,
    pub key: String,
    pub url: Option<String>,
}

impl JobTrigger {
    pub fn info(&self) -> TriggerInfo {
        TriggerInfo {
            rule: self.rule.name.clone(),
            source: self.event.source.clone(),
            kind: self.event.kind.clone(),
            key: self.event.key.clone(),
            url: self.event.url.clone(),
        }
    }

    /// The notification about a job of this trigger that finished in
    /// `state` with `text` (its answer or error).
    pub fn finished(&self, state: &str, text: &str) -> Notification {
        Notification {
            title: format!("{}: {state} — {}", self.rule.name, self.event.title),
            message: cut(text.trim(), MAX_RESULT_CHARS),
            priority: match state {
                "completed" => Priority::Normal,
                "cancelled" => Priority::Low,
                _ => Priority::High,
            },
            url: self.event.url.clone(),
            rule: Some(self.rule.name.clone()),
        }
    }
}

impl Automation {
    /// The automation of `config`, with the kinds of `registry`, triaging
    /// with `triage`. Sources and notifiers are built now, reading their
    /// tokens and URLs from the environment, so a missing one fails at
    /// startup.
    pub fn from_config(
        config: &AppConfig,
        registry: &AutomationRegistry,
        triage: ModelTarget,
    ) -> Result<Self> {
        let settings = config.automation.clone();
        let parts = settings.build(registry, &config.environments)?;
        let path = match &settings.state_file {
            Some(path) => path.clone(),
            None => default_state_file()
                .context("cannot find the OS data directory; set automation.state_file")?,
        };
        let store = AutomationStore::open(&path)?;
        Ok(Self::new(settings, store, parts, Triage::new(triage)))
    }

    pub fn new(
        settings: AutomationSettings,
        store: AutomationStore,
        parts: AutomationParts,
        triage: Triage,
    ) -> Self {
        Self {
            settings,
            store: Arc::new(store),
            sources: parts.sources,
            notifiers: parts.notifiers,
            triage,
            queued_events: Arc::new(Semaphore::new(MAX_QUEUED_EVENTS)),
            handling: Arc::new(Semaphore::new(MAX_CONCURRENT_EVENTS)),
        }
    }

    fn notifiers_of(&self, rule: &TriggerRule) -> Vec<NamedNotifier> {
        rule.notify
            .iter()
            .filter_map(|name| self.notifiers.get(name).cloned())
            .collect()
    }

    pub fn source(&self, name: &str) -> Option<&Arc<dyn EventSource>> {
        self.sources.get(name)
    }

    /// A slot to handle an event in, waiting at most `wait` for one.
    pub(super) async fn slot(&self, wait: Duration) -> Option<OwnedSemaphorePermit> {
        tokio::time::timeout(wait, Arc::clone(&self.handling).acquire_owned())
            .await
            .ok()?
            .ok()
    }
}

/// Record `event` as handled, unless it already was. Events that miss
/// what every event needs are refused, whichever source made them.
pub(super) fn accept(state: &WebhookState, event: &AutomationEvent) -> Admission {
    let Some(automation) = &state.automation else {
        return Admission::Failed("automation is not configured".into());
    };
    if let Err(error) = event.check() {
        return Admission::Failed(format!("invalid event: {error:#}"));
    }
    match automation.store.mark_seen(&event.source, &event.key) {
        Ok(true) => Admission::New,
        Ok(false) => Admission::Duplicate,
        Err(error) => Admission::Failed(format!("{error:#}")),
    }
}

/// Decide what to do with an event accepted in `slot`, and do it.
pub(super) async fn process_in(
    state: &Arc<WebhookState>,
    event: AutomationEvent,
    slot: OwnedSemaphorePermit,
) {
    if let Some(automation) = &state.automation {
        act(automation, state, event).await;
    }
    drop(slot);
}

/// Decide what to do with an accepted event, and do it.
async fn act(automation: &Automation, state: &Arc<WebhookState>, event: AutomationEvent) {
    let Some(rule) = matching_rule(&automation.settings.rules, &event) else {
        println!(
            "automation: {}/{} {}: no rule applies",
            event.source, event.kind, event.key
        );
        return;
    };
    let verdict = automation.triage.decide(rule, &event).await;
    println!(
        "automation: {}/{} {} → rule '{}': {} ({})",
        event.source,
        event.kind,
        event.key,
        rule.name,
        verdict.action.as_str(),
        verdict.reason
    );
    let notifiers = automation.notifiers_of(rule);
    match verdict.action {
        TriageAction::Ignore => {}
        TriageAction::Notify => {
            let notification = Notification {
                title: event.title.clone(),
                message: verdict.summary.clone(),
                priority: verdict.priority,
                url: event.url.clone(),
                rule: Some(rule.name.clone()),
            };
            send_all(&notifiers, &notification).await;
        }
        TriageAction::Run => {
            let (Some(task), Some(environment)) = (&rule.task, &rule.environment) else {
                return;
            };
            let request = event_run_request(task, &event);
            let mut approval_request = request.approval_request;
            if let Some(goal) = &rule.goal {
                approval_request.push_str(&format!("\n\nGoal: {goal}"));
            }
            let trigger = Arc::new(JobTrigger {
                rule: rule.clone(),
                event: event.clone(),
                notifiers: notifiers.clone(),
            });
            let spec = JobSpec {
                user: rule.user.clone().unwrap_or_else(|| "default".into()),
                environment: environment.clone(),
                input: vec![InputPart::Text(request.input)],
                goal: rule.goal.clone(),
                approval_request: Some(approval_request),
                trigger: Some(Arc::clone(&trigger)),
            };
            match admit_job(state, spec, None).await {
                Ok(id) => println!("automation: rule '{}' started job {id}", rule.name),
                Err((_, message)) => {
                    eprintln!(
                        "automation: rule '{}' could not start a job: {message}",
                        rule.name
                    );
                    let mut notification = trigger.finished("not started", &message);
                    notification.message = format!("The job could not start: {message}");
                    send_all(&notifiers, &notification).await;
                }
            }
        }
    }
}

/// The longest a timer sleeps before it looks at the clock again, so a
/// changed clock or a machine waking from sleep is noticed.
const MAX_TIMER_SLEEP: Duration = Duration::from_secs(60);
/// A time missed by more than this (the machine slept, or the server was
/// busy) is skipped instead of handled late.
const MAX_TIMER_LATENESS_SECS: u64 = 300;

/// Start a timer for every source that fires by itself at set times
/// (`schedule`). Each waits for its next time, hands the event to
/// automation like a posted one, and stops when the server shuts down. The
/// next time is always counted from now, so times that passed while the
/// server was stopped are not made up for.
pub(in crate::interface::webhook) fn start_timers(state: &Arc<WebhookState>) {
    let Some(automation) = &state.automation else {
        return;
    };
    let now = unix_now();
    for (name, source) in &automation.sources {
        let Some(next) = source.next_fire(now) else {
            continue;
        };
        println!(
            "automation: source '{name}' fires next at {}",
            local_time(next)
        );
        let (state, name, source) = (Arc::clone(state), name.clone(), Arc::clone(source));
        let mut tasks = state
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let task_state = Arc::clone(&state);
        tasks.spawn(async move { run_timer(&task_state, &name, source.as_ref()).await });
    }
}

async fn run_timer(state: &Arc<WebhookState>, name: &str, source: &dyn EventSource) {
    let mut shutdown = state.shutdown.subscribe();
    let mut after = unix_now();
    loop {
        if *shutdown.borrow() {
            return;
        }
        // A clock set back would put `after` in the future and skip the
        // times until it caught up; counting from now instead may meet a
        // handled time again, which its key turns into a duplicate.
        let Some(next) = source.next_fire(after.min(unix_now())) else {
            return;
        };
        let now = unix_now();
        if now < next {
            let wait = Duration::from_secs(next - now).min(MAX_TIMER_SLEEP);
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                _ = shutdown.changed() => return,
            }
            continue;
        }
        if now - next > MAX_TIMER_LATENESS_SECS {
            // Count from now, so missed times are never handled in a burst.
            println!(
                "automation: source '{name}' skipped {}, which passed while the server was busy or asleep",
                local_time(next)
            );
            after = now;
            continue;
        }
        after = next;
        for event in source.fired(name, next) {
            fire(state, event).await;
        }
    }
}

/// Hand an event a timer made to automation, as a posted one is.
async fn fire(state: &Arc<WebhookState>, event: AutomationEvent) {
    let Some(automation) = &state.automation else {
        return;
    };
    let source = event.source.clone();
    let key = event.key.clone();
    let Ok(queued) = Arc::clone(&automation.queued_events).try_acquire_owned() else {
        eprintln!("automation: {source}/{key} skipped: too many pending events");
        return;
    };
    let Some(slot) = automation.slot(TIMER_SLOT_WAIT).await else {
        eprintln!("automation: {source}/{key} skipped: busy with other events");
        return;
    };
    match accept(state, &event) {
        Admission::New => {
            process_in(state, event, slot).await;
        }
        Admission::Duplicate => {}
        Admission::Failed(error) => {
            eprintln!("automation: could not record {source}/{key}: {error}");
        }
    }
    drop(queued);
}

/// How long a timer waits for a slot to handle its event.
const TIMER_SLOT_WAIT: Duration = Duration::from_secs(60);

/// Send `notification` to every notifier, logging failures.
pub(super) async fn send_all(notifiers: &[NamedNotifier], notification: &Notification) {
    let results = join_all(notifiers.iter().map(|notifier| async move {
        (
            notifier.name.as_str(),
            notifier.notifier.send(notification).await,
        )
    }))
    .await;
    for (name, result) in results {
        if let Err(error) = result {
            eprintln!("automation: notifier '{name}' failed: {error:#}");
        }
    }
}
