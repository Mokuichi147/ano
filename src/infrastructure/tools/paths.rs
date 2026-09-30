//! Workspace paths: confining tool paths to the workspace, and writing
//! files there without following symbolic links.

use super::workspace::MAX_FILE_BYTES;
use crate::domain::tool::ToolContext;
use anyhow::{bail, Context, Result};
use std::{
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

pub(super) async fn commit_workspace_file(
    file: PathBuf,
    root: PathBuf,
    content: Vec<u8>,
    original: Option<Vec<u8>>,
    permissions: Option<std::fs::Permissions>,
    guard: tokio::sync::OwnedMutexGuard<()>,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        // Retain serialization even if the caller is cancelled while the
        // blocking filesystem operation is finishing.
        let _guard = guard;
        let parent = std::fs::canonicalize(file.parent().context("file has no parent")?)?;
        if !parent.starts_with(&root) {
            bail!("path escapes the configured workspace");
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&file) {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!("write target is not a regular file");
            }
            if metadata.permissions().readonly() {
                bail!("write target is read-only");
            }
        }
        if let Some(original) = original {
            use std::io::Read;
            let mut current = Vec::new();
            std::fs::File::open(&file)?
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut current)?;
            if current != original {
                bail!("edit conflict: file changed before save; read it again");
            }
        }
        crate::infrastructure::fs::atomic_write(&file, &content, permissions)
    })
    .await
    .context("workspace save task failed")?
}

pub(super) fn relative_path(raw_path: &str) -> Result<PathBuf> {
    let path = Path::new(raw_path);
    if raw_path.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("path must be a non-empty relative path without '..'");
    }
    Ok(path.to_path_buf())
}

pub(super) async fn workspace_root(context: &ToolContext) -> Result<PathBuf> {
    let root = context
        .workspace
        .as_ref()
        .context("no workspace is configured for this environment")?;
    tokio::fs::canonicalize(root)
        .await
        .with_context(|| format!("workspace does not exist: {}", root.display()))
}

pub(super) async fn existing_workspace_path(
    context: &ToolContext,
    relative: &Path,
) -> Result<PathBuf> {
    let root = workspace_root(context).await?;
    let candidate = tokio::fs::canonicalize(root.join(relative))
        .await
        .with_context(|| format!("workspace path does not exist: {}", relative.display()))?;
    if !candidate.starts_with(&root) {
        bail!("path escapes the configured workspace");
    }
    Ok(candidate)
}

/// Whether `relative` is inside repository metadata, where a written hook or
/// config (`core.fsmonitor`, filters) would run commands on the next git
/// call. Compared without case for case-insensitive filesystems.
pub(super) fn inside_git_dir(relative: &Path) -> bool {
    relative
        .components()
        .any(|component| component.as_os_str().eq_ignore_ascii_case(".git"))
}

/// Resolve a write target without ever touching the filesystem outside the
/// workspace: each parent directory is checked before the next one is
/// created, and the final path must not be a symbolic link. Paths inside
/// `.git` are refused.
pub(super) async fn writable_workspace_path(
    context: &ToolContext,
    relative: &Path,
) -> Result<PathBuf> {
    if inside_git_dir(relative) {
        bail!("files inside .git cannot be written");
    }
    let root = workspace_root(context).await?;
    let file_name = relative
        .file_name()
        .context("workspace_write path must name a file")?;

    let mut current = root.clone();
    for component in relative.parent().unwrap_or(Path::new("")).components() {
        let Component::Normal(part) = component else {
            continue;
        };
        let next = current.join(part);
        match tokio::fs::symlink_metadata(&next).await {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                tokio::fs::create_dir_all(&next).await?;
            }
            Err(error) => return Err(error.into()),
        }
        let canonical = tokio::fs::canonicalize(&next).await?;
        if !canonical.starts_with(&root) {
            bail!("path escapes the configured workspace");
        }
        if !tokio::fs::metadata(&canonical).await?.is_dir() {
            bail!(
                "workspace path component is not a directory: {}",
                part.to_string_lossy()
            );
        }
        current = canonical;
    }

    let target = current.join(file_name);
    match tokio::fs::symlink_metadata(&target).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing to write through a symbolic link: {}",
                relative.display()
            )
        }
        Ok(metadata) if metadata.is_dir() => {
            bail!("workspace path is a directory: {}", relative.display())
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::relative_path;

    #[test]
    fn rejects_absolute_and_parent_paths() {
        assert!(relative_path("../secret").is_err());
        assert!(relative_path("a/../../secret").is_err());
        assert!(relative_path("").is_err());
        assert!(relative_path(if cfg!(windows) {
            "C:\\x"
        } else {
            "/etc/passwd"
        })
        .is_err());
        assert!(relative_path("src/lib.rs").is_ok());
    }
}
