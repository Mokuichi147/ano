//! The state of automation that outlives a process: the events already
//! handled, so that a restart does not handle them again.

use super::fs::{acquire_lock, atomic_write};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Handled events remembered per source; the oldest are forgotten first.
const MAX_SEEN_PER_SOURCE: usize = 5000;
const STATE_VERSION: u32 = 1;

/// The default state file: `automation/state.json` in the OS data directory.
pub fn default_state_file() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ano")
        .map(|dirs| dirs.data_dir().join("automation").join("state.json"))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct AutomationState {
    version: u32,
    /// Per source, the keys of handled events with the order they came in.
    seen: BTreeMap<String, BTreeMap<String, u64>>,
    /// The order of the next handled event.
    next_order: u64,
}

/// The state file, locked for this process so that two servers never share
/// it.
pub struct AutomationStore {
    path: PathBuf,
    state: Mutex<AutomationState>,
    /// Handled events remembered per source.
    max_seen: usize,
    _lock: File,
}

impl AutomationStore {
    pub fn open(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .context("the state file has no parent directory")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let mut lock_name = path
            .file_name()
            .context("the state file has no file name")?
            .to_os_string();
        lock_name.push(".lock");
        let lock_path = path.with_file_name(lock_name);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open {}", lock_path.display()))?;
        match acquire_lock(&lock) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => bail!(
                "{} is in use by another ano serve; give each server its own automation.state_file",
                path.display()
            ),
            Err(TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("failed to lock {}", lock_path.display()))
            }
        }
        let state = match std::fs::read(path) {
            Ok(bytes) => {
                let state: AutomationState = serde_json::from_slice(&bytes)
                    .with_context(|| format!("invalid automation state {}", path.display()))?;
                if state.version > STATE_VERSION {
                    bail!(
                        "{} was written by a newer ano (state version {})",
                        path.display(),
                        state.version
                    );
                }
                state
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => AutomationState {
                version: STATE_VERSION,
                ..Default::default()
            },
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            state: Mutex::new(state),
            max_seen: MAX_SEEN_PER_SOURCE,
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record the event `key` of `source` as handled. Returns false when it
    /// already was.
    pub fn mark_seen(&self, source: &str, key: &str) -> Result<bool> {
        let mut first = false;
        let max_seen = self.max_seen;
        self.update(|state| {
            // A repeat changes nothing, so the file is not written for it.
            if state
                .seen
                .get(source)
                .is_some_and(|seen| seen.contains_key(key))
            {
                return false;
            }
            let order = state.next_order;
            let seen = state.seen.entry(source.to_string()).or_default();
            first = true;
            seen.insert(key.to_string(), order);
            state.next_order += 1;
            if seen.len() > max_seen {
                let mut orders: Vec<u64> = seen.values().copied().collect();
                orders.sort_unstable();
                let cutoff = orders[seen.len() - max_seen];
                seen.retain(|_, order| *order >= cutoff);
            }
            true
        })?;
        Ok(first)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AutomationState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Change the state and save it; `change` returns whether it changed
    /// anything, and nothing is written when it did not. The file is small,
    /// so it is written whole each time. A change that cannot be saved is
    /// not kept, so a failed save never marks an event as handled.
    fn update(&self, change: impl FnOnce(&mut AutomationState) -> bool) -> Result<()> {
        let mut state = self.lock();
        let mut changed = state.clone();
        if !change(&mut changed) {
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(&changed)?;
        atomic_write(&self.path, &bytes, None)?;
        *state = changed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automation").join("state.json");
        {
            let store = AutomationStore::open(&path).unwrap();
            assert!(store.mark_seen("gh", "issue:1").unwrap());
            assert!(!store.mark_seen("gh", "issue:1").unwrap());
            assert!(store.mark_seen("other", "issue:1").unwrap());
        }
        let store = AutomationStore::open(&path).unwrap();
        assert!(!store.mark_seen("gh", "issue:1").unwrap());
        assert!(!store.mark_seen("other", "issue:1").unwrap());
        assert!(store.mark_seen("gh", "issue:2").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_change_that_cannot_be_saved_is_not_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = AutomationStore::open(&dir.path().join("state.json")).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let failed = store.mark_seen("gh", "issue:1");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.is_err());
        assert!(store.mark_seen("gh", "issue:1").unwrap());
        // A repeat writes nothing, so it works while the directory cannot
        // be written.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let repeat = store.mark_seen("gh", "issue:1");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!repeat.unwrap());
    }

    #[test]
    fn a_second_server_cannot_share_the_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let _first = AutomationStore::open(&path).unwrap();
        let error = AutomationStore::open(&path).err().unwrap().to_string();
        assert!(error.contains("in use"), "{error}");
        // The lock stays apart from a state file named like a lock.
        let odd = dir.path().join("state.lock");
        let store = AutomationStore::open(&odd).unwrap();
        store.mark_seen("gh", "issue:1").unwrap();
        assert!(dir.path().join("state.lock.lock").exists());
        assert!(AutomationStore::open(&odd).is_err());
    }

    #[test]
    fn the_oldest_handled_events_are_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AutomationStore::open(&dir.path().join("state.json")).unwrap();
        store.max_seen = 3;
        for index in 0..=3 {
            store.mark_seen("gh", &format!("e{index}")).unwrap();
        }
        assert!(store.mark_seen("gh", "e0").unwrap());
        assert!(!store.mark_seen("gh", "e3").unwrap());
    }
}
