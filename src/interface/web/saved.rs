//! Sessions of the web UI kept across restarts: each in a folder of its
//! own, with `meta.json` (what the page asked for, the title, and times)
//! and `conversation.json` (the conversation, as `--session` files keep it).

use super::{events::UiEvent, session::NewSession};
use crate::infrastructure::fs::{atomic_write, user_directory};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{ErrorKind, Write},
    path::PathBuf,
};

const META_FILE: &str = "meta.json";
const CONVERSATION_FILE: &str = "conversation.json";
/// What was said, one JSON object per line: `{"role": "user" | "assistant",
/// "text": ...}`. The conversation file holds more, such as internal
/// notices, that the page never showed.
const TIMELINE_FILE: &str = "timeline.jsonl";
/// Longest title, in characters.
pub(super) const MAX_TITLE_CHARS: usize = 60;

/// `web-sessions` in the platform's data directory.
pub fn default_sessions_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ano").map(|dirs| dirs.data_dir().join("web-sessions"))
}

/// What is kept about a session besides its conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SavedMeta {
    pub(super) id: String,
    #[serde(default)]
    pub(super) title: String,
    pub(super) created_at_unix: u64,
    pub(super) updated_at_unix: u64,
    /// The request the session was made from, with its workspace resolved,
    /// so restoring checks its permissions again.
    pub(super) request: NewSession,
}

/// The saved sessions of one user.
pub(super) struct SavedSessions {
    root: PathBuf,
}

impl SavedSessions {
    /// The sessions of `user` under `dir`.
    pub(super) fn new(dir: &std::path::Path, user: &str) -> Result<Self> {
        Ok(Self {
            root: user_directory(dir, user)?,
        })
    }

    fn folder(&self, id: &str) -> Result<PathBuf> {
        // Ids are UUIDs the server made; anything else is not a folder here.
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            bail!("invalid session id {id:?}");
        }
        Ok(self.root.join(id))
    }

    pub(super) fn conversation_path(&self, id: &str) -> Result<PathBuf> {
        Ok(self.folder(id)?.join(CONVERSATION_FILE))
    }

    pub(super) fn write_meta(&self, meta: &SavedMeta) -> Result<()> {
        let folder = self.folder(&meta.id)?;
        std::fs::create_dir_all(&folder)
            .with_context(|| format!("failed to create {}", folder.display()))?;
        let mut bytes = serde_json::to_vec_pretty(meta)?;
        bytes.push(b'\n');
        atomic_write(&folder.join(META_FILE), &bytes, None)
    }

    /// Every saved session, most recently used first. Folders whose
    /// `meta.json` cannot be read are reported and left out.
    pub(super) fn list(&self) -> Vec<SavedMeta> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                eprintln!(
                    "warning: cannot read saved sessions in {}: {error}",
                    self.root.display()
                );
                return Vec::new();
            }
        };
        let mut sessions: Vec<SavedMeta> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| {
                let path = entry.path().join(META_FILE);
                let read = std::fs::read(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| Ok(serde_json::from_slice::<SavedMeta>(&bytes)?));
                match read {
                    Ok(meta) if entry.file_name().to_str() == Some(meta.id.as_str()) => Some(meta),
                    Ok(_) => {
                        eprintln!(
                            "warning: skipped {}: its id does not match its folder",
                            path.display()
                        );
                        None
                    }
                    Err(error) => {
                        eprintln!("warning: skipped {}: {error:#}", path.display());
                        None
                    }
                }
            })
            .collect();
        sessions.sort_by_key(|meta| std::cmp::Reverse(meta.updated_at_unix));
        sessions
    }

    /// Add what `role` (`user` or `assistant`) said to the session's
    /// timeline.
    pub(super) fn append_said(&self, id: &str, role: &str, text: &str) -> Result<()> {
        let path = self.folder(id)?.join(TIMELINE_FILE);
        let mut line = serde_json::to_string(&serde_json::json!({"role": role, "text": text}))?;
        line.push('\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .with_context(|| format!("failed to write {}", path.display()))
    }

    /// What was said in the session `id`, as the page shows it. Lines that
    /// cannot be read are skipped.
    pub(super) fn said(&self, id: &str) -> Result<Vec<UiEvent>> {
        let path = self.folder(id)?.join(TIMELINE_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        Ok(text
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|line| {
                let text = line["text"].as_str()?.to_string();
                match line["role"].as_str()? {
                    "user" => Some(UiEvent::UserMessage { text }),
                    "assistant" => Some(UiEvent::message(text)),
                    _ => None,
                }
            })
            .collect())
    }

    /// Delete the files of the session `id`.
    pub(super) fn remove(&self, id: &str) -> Result<()> {
        let folder = self.folder(id)?;
        match std::fs::remove_dir_all(&folder) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to delete {}", folder.display()))
            }
        }
    }
}

/// A title from the first message of a session: its first line, cut.
pub(super) fn title_from(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    crate::domain::text::cut_within(line, MAX_TITLE_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str, updated: u64) -> SavedMeta {
        SavedMeta {
            id: id.into(),
            title: String::new(),
            created_at_unix: 1,
            updated_at_unix: updated,
            request: NewSession::default(),
        }
    }

    #[test]
    fn sessions_are_listed_most_recent_first_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let saved = SavedSessions::new(dir.path(), "default").unwrap();
        assert!(saved.list().is_empty());
        saved.write_meta(&meta("0a-1", 5)).unwrap();
        saved.write_meta(&meta("0b-2", 9)).unwrap();
        std::fs::create_dir_all(dir.path().join("default/0c-3")).unwrap();
        std::fs::write(dir.path().join("default/0c-3/meta.json"), "{").unwrap();
        let ids: Vec<String> = saved.list().into_iter().map(|meta| meta.id).collect();
        assert_eq!(ids, ["0b-2", "0a-1"]);
        saved.remove("0b-2").unwrap();
        saved.remove("0b-2").unwrap();
        assert_eq!(saved.list().len(), 1);
        assert!(saved.conversation_path("../x").is_err());
        assert!(SavedSessions::new(dir.path(), "../x").is_err());
    }

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(
            title_from("\n  明日の予定を立てて \n詳細"),
            "明日の予定を立てて"
        );
        assert_eq!(
            title_from(&"x".repeat(100)).chars().count(),
            MAX_TITLE_CHARS
        );
        assert_eq!(title_from(" "), "");
    }
}
