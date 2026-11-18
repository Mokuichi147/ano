use anyhow::{bail, Context, Result};
use std::{
    fs::{File, Permissions, TryLockError},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

const LOCK_RETRIES: u32 = 25;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);

/// The directory of `user` under `root`, for data kept per user (skills,
/// the character). The user id becomes a path component, so it must be a
/// plain file name on every platform.
pub(crate) fn user_directory(root: &Path, user: &str) -> Result<PathBuf> {
    if user.is_empty()
        || user == "."
        || user == ".."
        || user.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
        })
    {
        bail!("user id {user:?} cannot be used as a directory name");
    }
    Ok(root.join(user))
}

/// Show `path` relative to the home directory when it is inside it.
pub fn display_path(path: &Path) -> String {
    match std::env::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf))
    {
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    }
}

/// Create `directory` if needed and hold the lock file `name` in it until
/// the returned file is dropped. Threads and processes wait for each other
/// briefly, then give up with an error.
pub(crate) fn lock_in(directory: &Path, name: &str) -> Result<File> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let path = directory.join(name);
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    acquire_lock(&lock).with_context(|| format!("{} is held by another writer", path.display()))?;
    Ok(lock)
}

/// Replace a file in one rename, leaving the previous file intact on failure.
/// Temporary files live beside the destination so the rename stays on one volume.
pub(crate) fn atomic_write(
    path: &Path,
    bytes: &[u8],
    permissions: Option<Permissions>,
) -> Result<()> {
    let parent = path.parent().context("file has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    if let Some(permissions) = permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to atomically save {}", path.display()))?;
    Ok(())
}

/// Take an exclusive lock on `lock`, retrying briefly. A child process being
/// spawned by this process can hold a copy of a just-released lock descriptor
/// until it execs (on macOS even through posix_spawn); a short retry avoids
/// reporting that window as another user. A lock that is really held stays
/// refused once the retries run out.
pub(crate) fn acquire_lock(lock: &File) -> std::result::Result<(), TryLockError> {
    let mut attempts = 0;
    loop {
        match lock.try_lock() {
            Err(TryLockError::WouldBlock) if attempts < LOCK_RETRIES => {
                attempts += 1;
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            result => return result,
        }
    }
}
