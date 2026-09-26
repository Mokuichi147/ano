//! Job states and the in-memory job table.

use crate::{
    application::agent::AgentResult,
    domain::{
        plan::TaskPlan,
        usage::{StopReason, UsageSummary},
    },
};
use serde::Serialize;
use std::{collections::HashMap, time::Instant};
use tokio::sync::watch;

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
}

pub(super) struct JobRecord {
    pub snapshot: JobStatus,
    pub cancel: watch::Sender<bool>,
    /// Monotonic completion order, including jobs finished in the same second.
    pub finished_at: Option<Instant>,
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
