//! Job states and the in-memory job table.

use crate::{
    application::agent::{AgentEvent, AgentResult},
    domain::{
        plan::TaskPlan,
        usage::{StopReason, UsageSummary},
    },
    interface::summarize_event,
};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::watch;

/// Events kept per job for `GET /jobs/<id>`.
const MAX_RECENT_EVENTS: usize = 50;
/// Longer event fields (tool arguments and outputs) are cut to a preview.
const MAX_EVENT_FIELD_CHARS: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Completed,
    Blocked,
    Incomplete,
    Failed,
    Cancelled,
    TimedOut,
}

impl JobState {
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Blocked
                | Self::Incomplete
                | Self::Failed
                | Self::Cancelled
                | Self::TimedOut
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub id: String,
    pub status: JobState,
    pub user: String,
    pub environment: String,
    pub created_at_unix: u64,
    pub started_at_unix: Option<u64>,
    pub finished_at_unix: Option<u64>,
    pub cancellation_requested: bool,
    pub result: Option<String>,
    pub plan: Option<TaskPlan>,
    pub usage: Option<UsageSummary>,
    pub stop_reason: Option<StopReason>,
    pub error: Option<String>,
    /// The latest agent events, oldest first, with large fields shortened.
    /// Available while the job runs, so progress is visible before it ends.
    pub recent_events: Vec<Value>,
}

/// Progress reported by the agent while a job runs. Updated from the event
/// listener, which cannot wait for the async job table lock.
#[derive(Default)]
pub(super) struct JobProgress {
    events: VecDeque<Value>,
    plan: Option<TaskPlan>,
    usage: Option<UsageSummary>,
}

impl JobProgress {
    pub fn record(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::PlanUpdated { plan, .. } => self.plan = Some(plan.clone()),
            AgentEvent::UsageUpdated { usage, .. } | AgentEvent::ExecutionStopped { usage, .. } => {
                self.usage = Some(usage.clone())
            }
            _ => {}
        }
        self.events
            .push_back(summarize_event(event, MAX_EVENT_FIELD_CHARS));
        if self.events.len() > MAX_RECENT_EVENTS {
            self.events.pop_front();
        }
    }
}

pub(super) struct JobRecord {
    pub snapshot: JobStatus,
    pub cancel: watch::Sender<bool>,
    /// Monotonic completion order, including jobs finished in the same second.
    pub finished_at: Option<Instant>,
    pub progress: Arc<Mutex<JobProgress>>,
}

impl JobRecord {
    /// The status returned to clients, including live progress. The final
    /// result's plan and usage take precedence over progress snapshots.
    pub fn status(&self) -> JobStatus {
        let mut status = self.snapshot.clone();
        let progress = self
            .progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        status.recent_events = progress.events.iter().cloned().collect();
        if status.plan.is_none() {
            status.plan.clone_from(&progress.plan);
        }
        if status.usage.is_none() {
            status.usage.clone_from(&progress.usage);
        }
        status
    }
}

pub(super) enum JobOutcome {
    Completed(Box<AgentResult>),
    Failed(String),
    Cancelled,
    TimedOut,
}

/// Drop the oldest finished jobs so at most `max_retained` remain.
pub(super) fn evict_finished_jobs(jobs: &mut HashMap<String, JobRecord>, max_retained: usize) {
    let mut finished = jobs
        .values()
        .filter(|job| job.snapshot.status.is_finished())
        .map(|job| (job.finished_at, job.snapshot.id.clone()))
        .collect::<Vec<_>>();
    let excess = finished.len().saturating_sub(max_retained);
    finished.sort();
    for (_, id) in finished.into_iter().take(excess) {
        jobs.remove(&id);
    }
}
