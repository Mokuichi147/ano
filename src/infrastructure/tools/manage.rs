//! `workspace_move` and `workspace_delete`: rearrange entries inside the
//! workspace. Both require `allow_writes`, never follow a final symbolic
//! link, and refuse the workspace root and anything inside `.git`.

use super::{
    non_strict_definition,
    workspace::{
        inside_git_dir, optional_bool, relative_path, workspace_root, writable_workspace_path,
    },
};
use crate::{application::registry::ToolRegistry, domain::tool::ToolContext};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs::Metadata,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

pub(super) fn register(registry: &ToolRegistry, mutations: Arc<Mutex<()>>) -> Result<()> {
    registry.register_contextual(
        non_strict_definition(
            "workspace_move",
            "Move or rename a file or directory inside the workspace. Missing destination directories are created. An existing destination file is replaced only with overwrite=true; directories are never replaced. Requires the environment to allow writes.",
            json!({
                "type": "object",
                "properties": {
                    "from": {"type": "string", "description": "Relative path of the existing entry"},
                    "to": {"type": "string", "description": "Relative destination path"},
                    "overwrite": {"type": "boolean", "description": "Replace an existing destination file; defaults to false"}
                },
                "required": ["from", "to"],
                "additionalProperties": false
            }),
        ),
        {
            let mutations = Arc::clone(&mutations);
            move |arguments, context| {
                let mutations = Arc::clone(&mutations);
                async move {
                    let guard = mutations.lock_owned().await;
                    workspace_move(arguments, &context, guard).await
                }
            }
        },
    )?;
    registry.register_contextual(
        non_strict_definition(
            "workspace_delete",
            "Delete a file, symbolic link, or empty directory inside the workspace. Set recursive=true to delete a directory and everything in it. This cannot be undone; confirm the path with workspace_list or workspace_find first. Requires the environment to allow writes.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative path to delete"},
                    "recursive": {"type": "boolean", "description": "Delete a non-empty directory with its contents; defaults to false"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        move |arguments, context| {
            let mutations = Arc::clone(&mutations);
            async move {
                let guard = mutations.lock_owned().await;
                workspace_delete(arguments, &context, guard).await
            }
        },
    )
}

fn require_writes(context: &ToolContext, tool: &str) -> Result<()> {
    if !context.allow_writes {
        bail!(
            "{tool} is disabled for environment '{}'; enable allow_writes",
            context.environment
        );
    }
    Ok(())
}

fn string_argument<'a>(arguments: &'a Value, tool: &str, name: &str) -> Result<&'a str> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("{tool}.{name} must be a string"))
}

/// Reject the workspace root itself and repository metadata.
fn protected_path(raw: &str) -> Result<PathBuf> {
    let relative = relative_path(raw)?;
    if relative.file_name().is_none() {
        bail!("path must name an entry inside the workspace, not the workspace itself");
    }
    if inside_git_dir(&relative) {
        bail!("entries inside .git cannot be moved or deleted");
    }
    Ok(relative)
}

/// Resolve an existing entry. Its parent is canonicalized and checked to be
/// inside the workspace; the entry itself is not followed if it is a link.
async fn existing_entry(context: &ToolContext, relative: &Path) -> Result<(PathBuf, Metadata)> {
    let root = workspace_root(context).await?;
    let parent = tokio::fs::canonicalize(root.join(relative.parent().unwrap_or(Path::new(""))))
        .await
        .with_context(|| format!("workspace path does not exist: {}", relative.display()))?;
    if !parent.starts_with(&root) {
        bail!("path escapes the configured workspace");
    }
    let entry = parent.join(relative.file_name().context("path must name an entry")?);
    let metadata = tokio::fs::symlink_metadata(&entry)
        .await
        .with_context(|| format!("workspace path does not exist: {}", relative.display()))?;
    Ok((entry, metadata))
}

fn kind(metadata: &Metadata) -> &'static str {
    if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_dir() {
        "directory"
    } else {
        "file"
    }
}

async fn workspace_move(
    arguments: Value,
    context: &ToolContext,
    guard: OwnedMutexGuard<()>,
) -> Result<Value> {
    require_writes(context, "workspace_move")?;
    let from = protected_path(string_argument(&arguments, "workspace_move", "from")?)?;
    let to = protected_path(string_argument(&arguments, "workspace_move", "to")?)?;
    let overwrite = optional_bool(&arguments, "overwrite")?;
    let (source, metadata) = existing_entry(context, &from).await?;
    let source_kind = kind(&metadata);
    // Creates missing parents inside the workspace; rejects a directory or a
    // symbolic link as the destination.
    let destination = writable_workspace_path(context, &to).await?;
    if destination == source {
        bail!("source and destination are the same path");
    }
    if metadata.is_dir() && destination.starts_with(&source) {
        bail!("a directory cannot be moved into itself");
    }
    let replaced = tokio::fs::symlink_metadata(&destination).await.is_ok();
    if replaced && !overwrite {
        bail!(
            "destination already exists: {}; set overwrite=true to replace it",
            to.display()
        );
    }
    if replaced && source_kind == "directory" {
        bail!("a directory cannot replace an existing file");
    }
    let (source_path, destination_path) = (source.clone(), destination.clone());
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        std::fs::rename(&source_path, &destination_path)
    })
    .await
    .context("workspace move task failed")?
    .with_context(|| format!("failed to move {} to {}", from.display(), to.display()))?;
    Ok(json!({
        "from": from, "to": to, "kind": source_kind, "moved": true, "replaced": replaced
    }))
}

async fn workspace_delete(
    arguments: Value,
    context: &ToolContext,
    guard: OwnedMutexGuard<()>,
) -> Result<Value> {
    require_writes(context, "workspace_delete")?;
    let relative = protected_path(string_argument(&arguments, "workspace_delete", "path")?)?;
    let recursive = optional_bool(&arguments, "recursive")?;
    let (entry, metadata) = existing_entry(context, &relative).await?;
    let entry_kind = kind(&metadata);
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        match entry_kind {
            // Removing a link never touches its target.
            "directory" if recursive => std::fs::remove_dir_all(&entry),
            "directory" => std::fs::remove_dir(&entry),
            _ => std::fs::remove_file(&entry),
        }
    })
    .await
    .context("workspace delete task failed")?
    .with_context(|| {
        if entry_kind == "directory" && !recursive {
            format!(
                "failed to delete {}; a non-empty directory requires recursive=true",
                relative.display()
            )
        } else {
            format!("failed to delete {}", relative.display())
        }
    })?;
    Ok(json!({"path": relative, "kind": entry_kind, "deleted": true}))
}

#[cfg(test)]
mod tests {
    use crate::{
        application::registry::ToolRegistry, domain::tool::ToolContext,
        infrastructure::tools::register_builtin_tools,
    };
    use serde_json::json;

    fn setup(allow_writes: bool) -> (tempfile::TempDir, ToolRegistry, ToolContext) {
        let workspace = tempfile::tempdir().unwrap();
        let registry = ToolRegistry::new();
        register_builtin_tools(&registry).unwrap();
        let context = ToolContext {
            workspace: Some(workspace.path().to_path_buf()),
            allow_writes,
            ..ToolContext::default()
        };
        (workspace, registry, context)
    }

    #[tokio::test]
    async fn moves_files_and_directories_without_silent_overwrites() {
        let (workspace, registry, context) = setup(true);
        let root = workspace.path();
        std::fs::create_dir_all(root.join("src/old")).unwrap();
        std::fs::write(root.join("src/old/a.txt"), "a").unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();

        let moved = registry
            .execute_with_context(
                "workspace_move",
                json!({"from":"src/old","to":"lib/new"}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(moved["kind"], "directory");
        assert_eq!(
            std::fs::read_to_string(root.join("lib/new/a.txt")).unwrap(),
            "a"
        );

        let conflict = json!({"from":"b.txt","to":"lib/new/a.txt"});
        assert!(registry
            .execute_with_context("workspace_move", conflict.clone(), &context)
            .await
            .is_err());
        let mut overwrite = conflict;
        overwrite["overwrite"] = json!(true);
        registry
            .execute_with_context("workspace_move", overwrite, &context)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("lib/new/a.txt")).unwrap(),
            "b"
        );
        assert!(!root.join("b.txt").exists());

        assert!(registry
            .execute_with_context(
                "workspace_move",
                json!({"from":"lib","to":"lib/inner/lib"}),
                &context
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn deletes_only_inside_the_workspace_and_protects_git() {
        let (workspace, registry, context) = setup(true);
        let root = workspace.path();
        std::fs::create_dir_all(root.join("build/out")).unwrap();
        std::fs::write(root.join("build/out/x.o"), "x").unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join("note.txt"), "n").unwrap();

        for path in [".git", ".git/config", ".", "../outside"] {
            assert!(
                registry
                    .execute_with_context("workspace_delete", json!({"path":path}), &context)
                    .await
                    .is_err(),
                "{path} should be refused"
            );
        }
        let error = registry
            .execute_with_context("workspace_delete", json!({"path":"build"}), &context)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("recursive=true"));
        registry
            .execute_with_context(
                "workspace_delete",
                json!({"path":"build","recursive":true}),
                &context,
            )
            .await
            .unwrap();
        assert!(!root.join("build").exists());
        registry
            .execute_with_context("workspace_delete", json!({"path":"note.txt"}), &context)
            .await
            .unwrap();
        assert!(!root.join("note.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deleting_a_link_keeps_its_target() {
        let (workspace, registry, context) = setup(true);
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep.txt"), "keep").unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("link")).unwrap();
        registry
            .execute_with_context(
                "workspace_delete",
                json!({"path":"link","recursive":true}),
                &context,
            )
            .await
            .unwrap();
        assert!(outside.path().join("keep.txt").exists());
        assert!(!workspace.path().join("link").exists());
    }

    #[tokio::test]
    async fn read_only_environments_cannot_move_or_delete() {
        let (workspace, registry, context) = setup(false);
        std::fs::write(workspace.path().join("a.txt"), "a").unwrap();
        for (name, arguments) in [
            ("workspace_move", json!({"from":"a.txt","to":"b.txt"})),
            ("workspace_delete", json!({"path":"a.txt"})),
        ] {
            assert!(registry
                .execute_with_context(name, arguments, &context)
                .await
                .is_err());
        }
        assert!(workspace.path().join("a.txt").exists());
    }
}
