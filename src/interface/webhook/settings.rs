//! `[webhook]` settings: listener, authentication, and job limits.

use anyhow::{bail, Result};
use serde::Deserialize;

fn default_webhook_bind() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_webhook_path() -> String {
    "/webhook/tasks".to_string()
}

fn default_webhook_secret_env() -> String {
    "ANO_WEBHOOK_SECRET".to_string()
}

fn default_webhook_max_body_bytes() -> usize {
    1_048_576
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookSettings {
    pub bind: String,
    pub path: String,
    pub secret_env: String,
    pub max_body_bytes: usize,
    /// Only honoured when the server is bound to a loopback address.
    pub allow_unauthenticated: bool,
    /// Accepted clock skew for `X-Ano-Timestamp`, and the replay window.
    pub signature_tolerance_secs: u64,
    /// Jobs that run at the same time. Further jobs wait in `queued`.
    pub max_concurrent_jobs: usize,
    /// Queued plus running jobs; new requests get 503 beyond this.
    pub max_pending_jobs: usize,
    /// Finished jobs kept for `GET /jobs/<id>`; the oldest are evicted.
    pub max_retained_jobs: usize,
    /// Wall-clock limit for a running job, excluding queue time.
    pub job_timeout_secs: u64,
}

impl Default for WebhookSettings {
    fn default() -> Self {
        Self {
            bind: default_webhook_bind(),
            path: default_webhook_path(),
            secret_env: default_webhook_secret_env(),
            max_body_bytes: default_webhook_max_body_bytes(),
            allow_unauthenticated: false,
            signature_tolerance_secs: 300,
            max_concurrent_jobs: 2,
            max_pending_jobs: 64,
            max_retained_jobs: 1000,
            job_timeout_secs: 1800,
        }
    }
}

impl WebhookSettings {
    pub fn validate(&self) -> Result<()> {
        if !self.path.starts_with('/') {
            bail!("webhook.path must start with '/'");
        }
        if self.path.contains(['{', '}', ':', '*', '?', '#'])
            || self.path == "/healthz"
            || self.path == "/jobs"
            || self.path.starts_with("/jobs/")
        {
            bail!("webhook.path must be a literal path outside the reserved /jobs and /healthz routes");
        }
        if self.max_concurrent_jobs == 0 || self.max_pending_jobs == 0 {
            bail!("webhook.max_concurrent_jobs and webhook.max_pending_jobs must be greater than zero");
        }
        if self.signature_tolerance_secs == 0 {
            bail!("webhook.signature_tolerance_secs must be greater than zero");
        }
        if self.job_timeout_secs == 0 {
            bail!("webhook.job_timeout_secs must be greater than zero");
        }
        Ok(())
    }
}
