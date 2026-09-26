use anyhow::{Context, Result};
use std::{fs::Permissions, io::Write, path::Path};

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
