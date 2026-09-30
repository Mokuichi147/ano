use anyhow::{Context, Result};
use std::{
    fs::{File, Permissions, TryLockError},
    io::Write,
    path::Path,
    time::Duration,
};

const LOCK_RETRIES: u32 = 25;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);

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
