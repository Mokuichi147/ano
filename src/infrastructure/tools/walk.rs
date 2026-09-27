//! Bounded, depth-first traversal of the workspace shared by search tools.

use super::workspace::existing_workspace_path;
use crate::domain::tool::ToolContext;
use ignore::{
    gitignore::{Gitignore, GitignoreBuilder},
    Match,
};
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

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
    /// Entries left out by `.gitignore` rules.
    pub ignored: usize,
}

/// The `.gitignore` files that apply in one directory, outermost first.
type IgnoreChain = Arc<Vec<Gitignore>>;

/// Walk `start` without following symbolic links or entering skipped
/// directories. `root` must be the canonical workspace root. With
/// `respect_ignore`, entries matched by the `.gitignore` files of the
/// workspace (and `.git/info/exclude`) are left out; `start` itself is
/// always walked.
pub(super) async fn walk_workspace(
    context: &ToolContext,
    root: &Path,
    start: &Path,
    respect_ignore: bool,
) -> Walk {
    let mut walk = Walk {
        entries: Vec::new(),
        truncated: false,
        unreadable: 0,
        ignored: 0,
    };
    let chain = if respect_ignore {
        ancestor_ignores(root, start).await
    } else {
        IgnoreChain::default()
    };
    let mut pending = vec![(start.to_path_buf(), chain)];
    let mut entries_seen = 1;
    while let Some((path, chain)) = pending.pop() {
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
        let chain = if respect_ignore {
            with_ignore_file(&chain, &absolute).await
        } else {
            chain
        };
        let mut directory = match tokio::fs::read_dir(&absolute).await {
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
            if is_ignored(&chain, &absolute.join(&name), kind.is_dir()) {
                walk.ignored += 1;
                continue;
            }
            children.push(path.join(name));
        }
        children.sort();
        pending.extend(
            children
                .into_iter()
                .rev()
                .map(|child| (child, Arc::clone(&chain))),
        );
    }
    walk
}

/// The ignore rules that apply inside `start`: `.git/info/exclude` and the
/// `.gitignore` files from the workspace root down to `start`'s parent.
/// Those of `start` itself are added when it is walked.
async fn ancestor_ignores(root: &Path, start: &Path) -> IgnoreChain {
    let mut chain = Vec::new();
    if let Some(exclude) = load_ignore(root, &root.join(".git/info/exclude")).await {
        chain.push(exclude);
    }
    let mut chain = Arc::new(chain);
    let mut directory = root.to_path_buf();
    let mut components = start
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .collect::<Vec<_>>();
    // `start` is the root: its rules are added when it is walked.
    if components.pop().is_none() {
        return chain;
    }
    for component in components {
        chain = with_ignore_file(&chain, &directory).await;
        directory.push(component);
    }
    with_ignore_file(&chain, &directory).await
}

/// `chain` plus the `.gitignore` of `directory`, if it has one.
async fn with_ignore_file(chain: &IgnoreChain, directory: &Path) -> IgnoreChain {
    match load_ignore(directory, &directory.join(".gitignore")).await {
        Some(ignore) => {
            let mut extended = Vec::clone(chain);
            extended.push(ignore);
            Arc::new(extended)
        }
        None => Arc::clone(chain),
    }
}

async fn load_ignore(base: &Path, file: &Path) -> Option<Gitignore> {
    let text = tokio::fs::read_to_string(file).await.ok()?;
    let mut builder = GitignoreBuilder::new(base);
    for line in text.lines() {
        // A malformed pattern is skipped, as git does.
        builder.add_line(None, line).ok();
    }
    builder.build().ok().filter(|ignore| !ignore.is_empty())
}

/// The deepest `.gitignore` with a matching rule decides, as in git.
fn is_ignored(chain: &[Gitignore], path: &Path, is_dir: bool) -> bool {
    for ignore in chain.iter().rev() {
        match ignore.matched(path, is_dir) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
    }
    false
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
