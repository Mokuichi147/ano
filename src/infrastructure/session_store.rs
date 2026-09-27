//! File-backed conversation store: one JSON checkpoint per session, a sidecar
//! lock against concurrent processes, and atomic replacement on every save.

use crate::{
    application::ports::ConversationStore,
    domain::{
        compaction::CompactionRecord,
        plan::TaskPlan,
        session::{SessionBinding, SessionData, SessionStatus},
        usage::UsageSummary,
    },
    infrastructure::fs::atomic_write,
};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_SESSION_BYTES: u64 = 32 * 1024 * 1024;
const LOCK_RETRIES: u32 = 25;
const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

pub struct Session {
    path: PathBuf,
    // Keep a separate inode locked across atomic replacements. Do not delete
    // this sidecar: unlinking it would let another process acquire a new lock.
    _lock: File,
    data: SessionData,
}

impl Session {
    pub fn open(path: impl AsRef<Path>, binding: SessionBinding, recover: bool) -> Result<Self> {
        let path = path.as_ref();
        let name = path.file_name().context("session path must name a file")?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let path = std::fs::canonicalize(parent)?.join(name);
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            bail!("session file must not be a symbolic link");
        }
        let mut lock_name = path.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(lock_name))?;
        acquire_lock(&lock)
            .with_context(|| format!("session is in use by another process: {}", path.display()))?;
        let data = match Self::inspect(&path) {
            Ok(data) => data,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == ErrorKind::NotFound) =>
            {
                SessionData::new(binding.clone(), now())
            }
            Err(error) => return Err(error),
        };
        if data.binding != binding {
            bail!("session belongs to a different user, environment, workspace, or API endpoint; use its original context or a new session file");
        }
        let mut session = Self {
            path,
            _lock: lock,
            data,
        };
        if session.data.status == SessionStatus::Running {
            if !recover {
                bail!("previous run was interrupted; inspect workspace changes, then use --recover-session to continue without replaying its pending tool calls");
            }
            session.fail("Previous process was interrupted. Tool side effects may have occurred; inspect current state before retrying any operation.")?;
        }
        Ok(session)
    }

    pub fn inspect(path: impl AsRef<Path>) -> Result<SessionData> {
        let file = File::open(path.as_ref())
            .with_context(|| format!("failed to open session {}", path.as_ref().display()))?;
        let mut bytes = Vec::new();
        file.take(MAX_SESSION_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SESSION_BYTES {
            bail!("session exceeds the 32 MiB limit");
        }
        let data: SessionData = serde_json::from_slice(&bytes)
            .context("invalid session file; original file was not modified")?;
        data.validate()?;
        Ok(data)
    }

    pub fn data(&self) -> &SessionData {
        &self.data
    }

    #[cfg(test)]
    pub(crate) fn record_tool_results(&mut self, results: &[Value]) -> Result<()> {
        self.data.record_tool_results(results);
        self.save()
    }

    fn save(&mut self) -> Result<()> {
        self.data.updated_at_unix = now();
        let bytes = serde_json::to_vec_pretty(&self.data)?;
        if bytes.len() as u64 > MAX_SESSION_BYTES {
            bail!("session exceeds the 32 MiB limit; start a new session");
        }
        atomic_write(&self.path, &bytes, None)
    }
}

impl ConversationStore for Session {
    fn data(&self) -> &SessionData {
        &self.data
    }

    fn begin_turn(&mut self, input: &Value) -> Result<()> {
        self.data.begin_turn(input)?;
        self.save()
    }

    fn record_response(&mut self, id: &str, output: &[Value]) -> Result<()> {
        self.data.record_response(id, output);
        // This must reach disk before executing any of these calls.
        self.save()
    }

    fn checkpoint_tool_result(&mut self, result: &Value, plan: &TaskPlan) -> Result<()> {
        self.data.checkpoint_tool_result(result, plan);
        self.save()
    }

    fn replace_plan(&mut self, plan: &TaskPlan) -> Result<()> {
        self.data.replace_plan(plan);
        self.save()
    }

    fn record_runtime_input(&mut self, input: &Value) -> Result<()> {
        self.data.record_runtime_input(input)?;
        self.save()
    }

    fn record_usage(&mut self, delta: &UsageSummary) -> Result<()> {
        self.data.record_usage(delta);
        self.save()
    }

    fn replace_history(
        &mut self,
        history: Vec<Value>,
        mut record: CompactionRecord,
    ) -> Result<CompactionRecord> {
        self.data.ensure_compactable()?;
        let original = self.data.clone();
        let archive = format!(
            "{}.archive-{}.json",
            self.path
                .file_name()
                .context("session path has no file name")?
                .to_string_lossy(),
            uuid::Uuid::new_v4()
        );
        // Write the complete previous checkpoint first. If the session save
        // fails, that session remains usable and the archive is still intact.
        atomic_write(
            &self.path.with_file_name(&archive),
            &serde_json::to_vec_pretty(&original)?,
            None,
        )?;
        record.archive_file = Some(archive);
        self.data.apply_compaction(history, record.clone())?;
        if let Err(error) = self.save() {
            self.data = original;
            return Err(error);
        }
        Ok(record)
    }

    fn skip_pending(&mut self, message: &str) -> Result<()> {
        self.data.skip_pending(message);
        self.save()
    }

    fn complete(&mut self) -> Result<()> {
        self.data.complete();
        self.save()
    }

    fn fail(&mut self, error: &str) -> Result<()> {
        self.data.fail(error)?;
        self.save()
    }
}

/// Take the sidecar lock, retrying briefly. A child process being spawned by
/// this process can hold a copy of a just-released lock descriptor until it
/// execs; a short retry avoids reporting that window as another user.
fn acquire_lock(lock: &File) -> std::result::Result<(), std::fs::TryLockError> {
    let mut attempts = 0;
    loop {
        match lock.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) if attempts < LOCK_RETRIES => {
                attempts += 1;
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            result => return result,
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{compaction::compacted_history, tool::ToolContext};
    use serde_json::json;

    fn binding() -> SessionBinding {
        SessionBinding::new(
            &ToolContext {
                user_id: "alice".into(),
                environment: "review".into(),
                ..Default::default()
            },
            "http://127.0.0.1:1234/v1/",
        )
        .unwrap()
    }

    #[test]
    fn saves_all_output_items_and_reopens_between_turns() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("work.json");
        let output = vec![
            json!({"type":"reasoning", "id":"reasoning1", "encrypted_content":"encrypted", "summary":[]}),
            json!({"type":"message", "role":"assistant", "phase":"final_answer", "content":[{"type":"output_text","text":"done"}]}),
        ];
        {
            let mut session = Session::open(&path, binding(), false).unwrap();
            session
                .begin_turn(&json!([{"role":"user", "content":"remember this"}]))
                .unwrap();
            session.record_response("response1", &output).unwrap();
            session.record_tool_results(&[]).unwrap();
            session.complete().unwrap();
        }
        let session = Session::open(&path, binding(), false).unwrap();
        assert_eq!(session.data.status, SessionStatus::Ready);
        assert_eq!(session.data.completed_turns, 1);
        assert_eq!(session.data.history[1..], output);
        assert_eq!(session.data.binding.endpoint, "http://127.0.0.1:1234/v1");
    }

    #[test]
    fn locks_sessions_and_recovers_interrupted_calls_without_reexecution() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("work.json");
        {
            let mut session = Session::open(&path, binding(), false).unwrap();
            assert!(Session::open(&path, binding(), false).is_err());
            session
                .begin_turn(&json!([{"role":"user","content":"edit"}]))
                .unwrap();
            session.record_response("response1", &[
                json!({"type":"function_call","call_id":"write1","name":"workspace_edit","arguments":"{}"}),
                json!({"type":"mcp_approval_request","id":"approve1","server_label":"docs","name":"update","arguments":"{}"}),
            ]).unwrap();
            assert_eq!(Session::inspect(&path).unwrap().pending_calls().len(), 2);
        }
        assert!(Session::open(&path, binding(), false).is_err());
        let session = Session::open(&path, binding(), true).unwrap();
        assert_eq!(session.data.status, SessionStatus::Failed);
        assert!(session.data.pending_calls().is_empty());
        assert_eq!(session.data.history[3]["call_id"], "write1");
        let output: Value =
            serde_json::from_str(session.data.history[3]["output"].as_str().unwrap()).unwrap();
        assert_eq!(output["error"], "execution_interrupted");
        assert_eq!(session.data.history[4]["approval_request_id"], "approve1");
        assert_eq!(session.data.history[4]["approve"], false);
    }

    #[test]
    fn prevents_cross_context_reuse_and_preserves_corrupt_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("work.json");
        {
            let mut session = Session::open(&path, binding(), false).unwrap();
            session
                .begin_turn(&json!([{"role":"user","content":"private"}]))
                .unwrap();
            session.complete().unwrap();
        }
        let mut other = binding();
        other.user_id = "bob".into();
        assert!(Session::open(&path, other, false).is_err());
        let mut other = binding();
        other.endpoint = "https://example.invalid/v1".into();
        assert!(Session::open(&path, other, false).is_err());
        let mut other = binding();
        other.environment = "another".into();
        assert!(Session::open(&path, other, false).is_err());
        std::fs::write(&path, "broken file").unwrap();
        assert!(Session::open(&path, binding(), true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken file");
    }

    #[test]
    fn failed_turn_keeps_known_tool_results() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("work.json");
        let mut session = Session::open(&path, binding(), false).unwrap();
        session
            .begin_turn(&json!([{"role":"user","content":"edit"}]))
            .unwrap();
        session.record_response("r1", &[json!({"type":"function_call","call_id":"c1","name":"workspace_edit","arguments":"{}"})]).unwrap();
        session
            .record_tool_results(&[
                json!({"type":"function_call_output","call_id":"c1","output":"saved"}),
            ])
            .unwrap();
        session.fail("API unavailable").unwrap();
        let saved = Session::inspect(&path).unwrap();
        assert_eq!(
            saved
                .history
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .count(),
            1
        );
        assert_eq!(saved.history[2]["output"], "saved");
        assert_eq!(saved.status, SessionStatus::Failed);
    }

    #[test]
    fn sessions_from_before_task_plans_remain_readable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.json");
        {
            let mut session = Session::open(&path, binding(), false).unwrap();
            session
                .begin_turn(&json!([{"role":"user","content":"original task"}]))
                .unwrap();
            session.complete().unwrap();
        }
        let mut old: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("plan");
        old.as_object_mut().unwrap().remove("usage");
        old.as_object_mut().unwrap().remove("compactions");
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let reopened = Session::open(&path, binding(), false).unwrap();
        assert_eq!(reopened.data.plan, TaskPlan::default());
        assert_eq!(reopened.data.usage, UsageSummary::default());
        assert!(reopened.data.compactions.is_empty());
        assert_eq!(reopened.data.history[0]["content"], "original task");
    }

    #[test]
    fn compaction_rejects_pending_calls_before_creating_an_archive() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("work.json");
        let mut session = Session::open(&path, binding(), false).unwrap();
        session
            .begin_turn(&json!([{"role":"user","content":"task"}]))
            .unwrap();
        session.record_response("r1", &[json!({"type":"function_call","call_id":"write1","name":"workspace_write","arguments":"{}"})]).unwrap();
        let before = std::fs::read(&path).unwrap();
        let (history, record) = compacted_history(&json!({"object":"response.compaction","id":"cmp1","output":[{"type":"compaction","encrypted_content":"opaque"}]}), &session.data.history).unwrap();
        assert!(session.replace_history(history, record).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(session.data.compactions.is_empty());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2); // checkpoint and lock
    }

    #[test]
    fn failed_compaction_save_preserves_original_checkpoint_and_memory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("work.json");
        let mut session = Session::open(&path, binding(), false).unwrap();
        session
            .begin_turn(&json!([{"role":"user","content":"task"}]))
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let original = session.data.history.clone();
        let (history, record) = compacted_history(&json!({"object":"response.compaction","id":"cmp1","output":[{"type":"compaction","encrypted_content":"x".repeat(MAX_SESSION_BYTES as usize)}]}), &original).unwrap();
        assert!(session.replace_history(history, record).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(session.data.history, original);
        assert!(session.data.compactions.is_empty());
        let archive = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().contains("archive-"))
            .unwrap();
        assert_eq!(Session::inspect(archive.path()).unwrap().history, original);
    }
}
