//! Child processes with a deadline and bounded output, shared by
//! `workspace_check` and `workspace_exec`.

use anyhow::{Context, Result};
use std::{
    collections::VecDeque,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

/// Bytes kept from the start of a stream.
const HEAD_BYTES: usize = 16 * 1024;
/// Bytes kept from the end of a stream. Build and test failures are usually
/// reported last, so the end gets most of the budget.
const TAIL_BYTES: usize = 48 * 1024;
/// How long to wait for the pipes to close after the process was stopped.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The beginning and the end of one output stream.
#[derive(Debug, Default)]
pub(super) struct BoundedOutput {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}

impl BoundedOutput {
    fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len());
        let head_room = HEAD_BYTES.saturating_sub(self.head.len()).min(bytes.len());
        let (head, rest) = bytes.split_at(head_room);
        self.head.extend_from_slice(head);
        self.tail.extend(rest);
        let excess = self.tail.len().saturating_sub(TAIL_BYTES);
        self.tail.drain(..excess);
    }

    pub(super) fn truncated(&self) -> bool {
        self.total > self.head.len() + self.tail.len()
    }

    /// The kept text, with a marker where bytes were left out.
    pub(super) fn text(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.head).into_owned();
        if self.truncated() {
            let omitted = self.total - self.head.len() - self.tail.len();
            text.push_str(&format!("\n… [{omitted} bytes omitted] …\n"));
        }
        let (first, second) = self.tail.as_slices();
        text.push_str(&String::from_utf8_lossy(&[first, second].concat()));
        text
    }
}

pub(super) struct ProcessOutput {
    /// `None` when the deadline passed first.
    pub status: Option<ExitStatus>,
    pub stdout: BoundedOutput,
    pub stderr: BoundedOutput,
}

impl ProcessOutput {
    pub(super) fn timed_out(&self) -> bool {
        self.status.is_none()
    }

    pub(super) fn success(&self) -> bool {
        self.status.is_some_and(|status| status.success())
    }

    pub(super) fn exit_code(&self) -> Option<i32> {
        self.status.and_then(|status| status.code())
    }
}

/// Run `command` with no stdin until it exits or `timeout` passes.
///
/// On Unix the command gets its own process group, which is stopped when the
/// command finishes, times out, or the returned future is dropped (for
/// example when the user cancels the turn). Background processes the command
/// started therefore do not outlive it or keep its output pipes open.
pub(super) async fn run_bounded(mut command: Command, timeout: Duration) -> Result<ProcessOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let mut child = command.spawn()?;
    let group = ProcessGroup(child.id());
    let stdout = child.stdout.take().context("process stdout unavailable")?;
    let stderr = child.stderr.take().context("process stderr unavailable")?;
    let stdout = Capture::spawn(stdout);
    let stderr = Capture::spawn(stderr);

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(error)) => {
            child.kill().await.ok();
            return Err(error).context("failed to wait for the process");
        }
        Err(_) => {
            child.kill().await.ok();
            None
        }
    };
    drop(group);
    Ok(ProcessOutput {
        status,
        stdout: stdout.finish().await,
        stderr: stderr.finish().await,
    })
}

/// Reads one pipe to the end in the background, so a verbose child never
/// blocks on a full pipe.
struct Capture {
    output: Arc<Mutex<BoundedOutput>>,
    task: tokio::task::JoinHandle<()>,
}

impl Capture {
    fn spawn(mut reader: impl AsyncRead + Unpin + Send + 'static) -> Self {
        let output = Arc::new(Mutex::new(BoundedOutput::default()));
        let shared = Arc::clone(&output);
        let task = tokio::spawn(async move {
            let mut buffer = [0_u8; 8192];
            while let Ok(read @ 1..) = reader.read(&mut buffer).await {
                shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(&buffer[..read]);
            }
        });
        Self { output, task }
    }

    /// Wait briefly for the pipe to close and return what was read. A
    /// process that escaped the group can hold the pipe open indefinitely.
    async fn finish(self) -> BoundedOutput {
        let Self { output, mut task } = self;
        if tokio::time::timeout(DRAIN_GRACE, &mut task).await.is_err() {
            task.abort();
        }
        let mut output = output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *output)
    }
}

/// Stops the process group led by the child when dropped.
#[cfg_attr(not(unix), allow(dead_code))]
struct ProcessGroup(Option<u32>);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(id) = self.0.and_then(|id| libc::pid_t::try_from(id).ok()) {
            // SAFETY: killpg only sends a signal. The group id is the child's
            // pid, which stays reserved while the group has members.
            unsafe {
                libc::killpg(id, libc::SIGKILL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_beginning_and_the_end_of_long_output() {
        let mut output = BoundedOutput::default();
        output.push(b"start\n");
        for _ in 0..HEAD_BYTES + TAIL_BYTES {
            output.push(b"x");
        }
        output.push(b"\nerror: the end");
        assert!(output.truncated());
        let text = output.text();
        assert!(text.starts_with("start\n"));
        assert!(text.ends_with("\nerror: the end"));
        assert!(text.contains("bytes omitted"));
        assert!(text.len() < HEAD_BYTES + TAIL_BYTES + 100);

        let mut short = BoundedOutput::default();
        short.push(b"all of it");
        assert!(!short.truncated());
        assert_eq!(short.text(), "all of it");
    }
}
