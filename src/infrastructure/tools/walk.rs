//! Bounded, depth-first traversal of the workspace shared by search tools.

use super::workspace::existing_workspace_path;
use crate::domain::tool::ToolContext;
use std::path::{Path, PathBuf};

pub(super) const MAX_WALK_ENTRIES: usize = 10_000;

/// Directories that are generated or tool-owned, never worth searching.
const SKIPPED_DIRECTORIES: &[&str] = &[".git", ".ano", "target", "node_modules", ".venv"];

pub(super) struct WalkEntry {
    /// Path relative to the workspace root, as addressed by the tools.
    pub path: PathBuf,
    /// Canonical path, already checked to be inside the workspace.
    pub absolute: PathBuf,
    pub is_dir: bool,
    pub bytes: u64,
}

pub(super) struct Walk {
    /// Entries under `start` in sorted depth-first order, excluding `start`.
    pub entries: Vec<WalkEntry>,
    /// The entry limit stopped the traversal early.
    pub truncated: bool,
    pub unreadable: usize,
}

/// Walk `start` without following symbolic links or entering skipped
/// directories. `root` must be the canonical workspace root.
pub(super) async fn walk_workspace(context: &ToolContext, root: &Path, start: &Path) -> Walk {
    let mut walk = Walk {
        entries: Vec::new(),
        truncated: false,
        unreadable: 0,
    };
    let mut pending = vec![start.to_path_buf()];
    let mut entries_seen = 1;
    while let Some(path) = pending.pop() {
        let metadata = match tokio::fs::symlink_metadata(root.join(&path)).await {
            Ok(metadata) => metadata,
            Err(_) => {
                walk.unreadable += 1;
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        // Recheck each resolved path, including any intermediate components
        // in an explicitly supplied start path.
        let absolute = match existing_workspace_path(context, &path).await {
            Ok(absolute) => absolute,
            Err(_) => {
                walk.unreadable += 1;
                continue;
            }
        };
        if !metadata.is_dir() {
            if metadata.is_file() {
                walk.entries.push(WalkEntry {
                    path,
                    absolute,
                    is_dir: false,
                    bytes: metadata.len(),
                });
            }
            continue;
        }
        if path != start {
            walk.entries.push(WalkEntry {
                path: path.clone(),
                absolute: absolute.clone(),
                is_dir: true,
                bytes: 0,
            });
        }
        let mut directory = match tokio::fs::read_dir(absolute).await {
            Ok(directory) => directory,
            Err(_) => {
                walk.unreadable += 1;
                continue;
            }
        };
        let mut children = Vec::new();
        loop {
            let entry = match directory.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => {
                    walk.unreadable += 1;
                    break;
                }
            };
            if entries_seen >= MAX_WALK_ENTRIES {
                walk.truncated = true;
                break;
            }
            entries_seen += 1;
            let kind = match entry.file_type().await {
                Ok(kind) => kind,
                Err(_) => {
                    walk.unreadable += 1;
                    continue;
                }
            };
            let name = entry.file_name();
            if kind.is_symlink()
                || (kind.is_dir()
                    && name
                        .to_str()
                        .is_some_and(|name| SKIPPED_DIRECTORIES.contains(&name)))
            {
                continue;
            }
            children.push(path.join(name));
        }
        children.sort();
        pending.extend(children.into_iter().rev());
    }
    walk
}

/// `path` relative to `start`, joined with `/` for glob matching.
pub(super) fn relative_to(start: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(start).unwrap_or(path);
    let relative = if relative.as_os_str().is_empty() {
        Path::new(path.file_name().unwrap_or_default())
    } else {
        relative
    };
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
