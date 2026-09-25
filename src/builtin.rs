//! Built-in local tools: `echo`, `unix_time`, and workspace file access.
//!
//! Workspace tools are confined to `ToolContext::workspace`. Paths must be
//! relative, every existing ancestor is canonicalized and checked against the
//! workspace root, and writes never follow a symbolic link.

use crate::tools::{ToolContext, ToolDefinition, ToolRegistry};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const DEFAULT_READ_BYTES: u64 = 64 * 1024;
const MAX_LIST_ENTRIES: usize = 1000;
const MAX_SEARCH_ENTRIES: usize = 10_000;
const MAX_SEARCH_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SEARCH_LINE_BYTES: usize = 2000;

/// Register the built-in tools into `registry`.
pub fn register_builtin_tools(registry: &ToolRegistry) -> Result<()> {
    registry.register(
        non_strict_definition(
            "echo",
            "Return the supplied JSON value unchanged. Useful for testing tool wiring.",
            json!({
                "type": "object",
                "properties": {"value": {}},
                "required": ["value"],
                "additionalProperties": false
            }),
        ),
        |arguments| async move {
            arguments
                .get("value")
                .cloned()
                .context("echo.value is required")
        },
    )?;
    registry.register(
        ToolDefinition::new(
            "unix_time",
            "Return the current Unix timestamp.",
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        ),
        |_arguments| async move {
            let seconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock is before Unix epoch")?
                .as_secs();
            Ok(json!({"unix_seconds": seconds}))
        },
    )?;
    registry.register_contextual(
        non_strict_definition(
            "workspace_list",
            "List directory entries in name order. Pass next_after as after to retrieve the next page.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative directory path; defaults to ."},
                    "after": {"type": "string", "description": "Continue after this entry name, returned as next_after"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIST_ENTRIES, "description": "Entries per page; defaults to 100"}
                },
                "required": [],
                "additionalProperties": false
            }),
        ),
        |arguments, context| async move { workspace_list(arguments, &context).await },
    )?;
    registry.register_contextual(
        non_strict_definition(
            "workspace_read",
            "Read a UTF-8 text file in bounded pages (64 KiB by default). Continue from next_offset when truncated; offsets count bytes.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative file path"},
                    "offset": {"type": "integer", "minimum": 0, "description": "Byte offset from a previous next_offset; defaults to 0"},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_FILE_BYTES}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        |arguments, context| async move { workspace_read(arguments, &context).await },
    )?;
    registry.register_contextual(
        non_strict_definition(
            "workspace_search",
            "Find literal text in workspace UTF-8 files and return paths, line numbers, and excerpts. Searches recursively, skips symlinks and .git/target/node_modules/.venv directories, and limits work to 10000 entries and 32 MiB. Narrow path if truncated.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1, "description": "Case-sensitive literal text on one line"},
                    "path": {"type": "string", "description": "Relative file or directory path; defaults to ."},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Matching lines to return; defaults to 100"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        |arguments, context| async move { workspace_search(arguments, &context).await },
    )?;
    registry.register_contextual(
        ToolDefinition::new(
            "workspace_write",
            "Write UTF-8 text to a file under the selected workspace. Requires the environment to allow writes.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative file path"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        ),
        |arguments, context| async move { workspace_write(arguments, &context).await },
    )?;
    Ok(())
}

// Optional arguments and echo's arbitrary JSON value are intentionally not
// strict Responses schemas. Runtime handlers validate the optional arguments.
fn non_strict_definition(name: &str, description: &str, parameters: Value) -> ToolDefinition {
    let mut definition = ToolDefinition::new(name, description, parameters);
    definition.strict = false;
    definition
}

fn optional_string<'a>(arguments: &'a Value, name: &str, default: &'a str) -> Result<&'a str> {
    match arguments
        .as_object()
        .context("arguments must be an object")?
        .get(name)
    {
        None => Ok(default),
        Some(value) => value
            .as_str()
            .with_context(|| format!("{name} must be a string")),
    }
}

fn optional_integer(
    arguments: &Value,
    name: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64> {
    let value = match arguments
        .as_object()
        .context("arguments must be an object")?
        .get(name)
    {
        None => default,
        Some(value) => value
            .as_u64()
            .with_context(|| format!("{name} must be a non-negative integer"))?,
    };
    if !(min..=max).contains(&value) {
        bail!("{name} must be between {min} and {max}");
    }
    Ok(value)
}

async fn workspace_list(arguments: Value, context: &ToolContext) -> Result<Value> {
    let relative = relative_path(optional_string(&arguments, "path", ".")?)?;
    let after = OsString::from(optional_string(&arguments, "after", "")?);
    let limit = optional_integer(&arguments, "limit", 100, 1, MAX_LIST_ENTRIES as u64)? as usize;
    let directory = existing_workspace_path(context, &relative).await?;
    if !tokio::fs::metadata(&directory).await?.is_dir() {
        bail!("workspace path is not a directory: {}", relative.display());
    }

    let mut entries = tokio::fs::read_dir(&directory).await?;
    // Keep only the next page plus one entry, so even very large directories
    // do not require collecting every entry in memory. read_dir order is not
    // stable across calls; sort before applying the page boundary.
    let mut page = BTreeMap::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        if name <= after {
            continue;
        }
        page.insert(name, entry);
        if page.len() > limit + 1 {
            page.pop_last();
        }
    }
    let truncated = page.len() > limit;
    if truncated {
        page.pop_last();
    }
    let next_after = if truncated {
        page.last_key_value()
            .map(|(name, _)| name.to_string_lossy().into_owned())
    } else {
        None
    };
    let mut result = Vec::with_capacity(page.len());
    for (name, entry) in page {
        let file_type = entry.file_type().await?;
        let kind = if file_type.is_symlink() {
            "symlink"
        } else if file_type.is_dir() {
            "directory"
        } else {
            "file"
        };
        let bytes = entry.metadata().await.map(|metadata| metadata.len()).ok();
        result.push(json!({
            "name": name.to_string_lossy(),
            "kind": kind,
            "bytes": bytes,
        }));
    }
    Ok(
        json!({"path": relative, "entries": result, "truncated": truncated, "next_after": next_after}),
    )
}

async fn workspace_read(arguments: Value, context: &ToolContext) -> Result<Value> {
    let raw_path = arguments
        .get("path")
        .and_then(Value::as_str)
        .context("workspace_read.path must be a string")?;
    let relative = relative_path(raw_path)?;
    let max_bytes = optional_integer(
        &arguments,
        "max_bytes",
        DEFAULT_READ_BYTES,
        1,
        MAX_FILE_BYTES,
    )?;
    let offset = optional_integer(&arguments, "offset", 0, 0, u64::MAX)?;
    let file = existing_workspace_path(context, &relative).await?;
    let metadata = tokio::fs::metadata(&file).await?;
    if !metadata.is_file() {
        bail!("workspace path is not a file: {}", relative.display());
    }
    if offset > metadata.len() {
        bail!("offset exceeds the {} byte file size", metadata.len());
    }
    let mut file = tokio::fs::File::open(file).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes).await?;
    let truncated = bytes.len() as u64 > max_bytes;
    if truncated {
        bytes.truncate(max_bytes as usize);
    }
    let valid_length = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(error) if truncated && error.error_len().is_none() => error.valid_up_to(),
        Err(_) => bail!(
            "workspace file is not valid UTF-8 or offset is not at a UTF-8 character boundary"
        ),
    };
    if truncated && valid_length == 0 {
        bail!("max_bytes is too small for the next UTF-8 character; use at least 4");
    }
    bytes.truncate(valid_length);
    let content = String::from_utf8(bytes).context("workspace file is not valid UTF-8")?;
    let next_offset = truncated.then_some(offset + content.len() as u64);
    Ok(json!({
        "path": relative, "bytes": content.len(), "content": content,
        "offset": offset, "total_bytes": metadata.len(), "truncated": truncated,
        "next_offset": next_offset
    }))
}

async fn workspace_search(arguments: Value, context: &ToolContext) -> Result<Value> {
    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .context("workspace_search.query must be a string")?;
    if query.is_empty() || query.contains(['\r', '\n']) {
        bail!("workspace_search.query must be non-empty text on one line");
    }
    let relative = relative_path(optional_string(&arguments, "path", ".")?)?;
    let max_results = optional_integer(&arguments, "max_results", 100, 1, 1000)? as usize;
    let root = workspace_root(context).await?;
    existing_workspace_path(context, &relative).await?;
    let mut pending = vec![relative.clone()];
    let mut matches = Vec::new();
    let mut entries_seen = 1;
    let mut files_searched = 0;
    let mut bytes_read = 0;
    let mut skipped_binary = 0;
    let mut skipped_large = 0;
    let mut skipped_unreadable = 0;
    let mut truncated = false;

    'search: while let Some(path) = pending.pop() {
        let metadata = match tokio::fs::symlink_metadata(root.join(&path)).await {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped_unreadable += 1;
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        // Recheck each resolved path before reading, including any intermediate
        // components in an explicitly supplied path.
        let absolute = match existing_workspace_path(context, &path).await {
            Ok(absolute) => absolute,
            Err(_) => {
                skipped_unreadable += 1;
                continue;
            }
        };
        if metadata.is_dir() {
            let mut directory = match tokio::fs::read_dir(absolute).await {
                Ok(directory) => directory,
                Err(_) => {
                    skipped_unreadable += 1;
                    continue;
                }
            };
            let mut children = Vec::new();
            loop {
                let entry = match directory.next_entry().await {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    Err(_) => {
                        skipped_unreadable += 1;
                        break;
                    }
                };
                if entries_seen >= MAX_SEARCH_ENTRIES {
                    truncated = true;
                    break;
                }
                entries_seen += 1;
                let kind = match entry.file_type().await {
                    Ok(kind) => kind,
                    Err(_) => {
                        skipped_unreadable += 1;
                        continue;
                    }
                };
                let name = entry.file_name();
                if kind.is_symlink()
                    || (kind.is_dir()
                        && matches!(
                            name.to_str(),
                            Some(".git" | "target" | "node_modules" | ".venv")
                        ))
                {
                    continue;
                }
                children.push(path.join(name));
            }
            children.sort();
            pending.extend(children.into_iter().rev());
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > MAX_FILE_BYTES {
            skipped_large += 1;
            continue;
        }
        if metadata.len() > MAX_SEARCH_BYTES - bytes_read {
            truncated = true;
            break;
        }
        let file = match tokio::fs::File::open(absolute).await {
            Ok(file) => file,
            Err(_) => {
                skipped_unreadable += 1;
                continue;
            }
        };
        let limit = MAX_FILE_BYTES.min(MAX_SEARCH_BYTES - bytes_read);
        let mut bytes = Vec::new();
        if file.take(limit + 1).read_to_end(&mut bytes).await.is_err() {
            bytes_read += bytes.len() as u64;
            skipped_unreadable += 1;
            if bytes_read >= MAX_SEARCH_BYTES {
                truncated = true;
                break;
            }
            continue;
        }
        if bytes.len() as u64 > limit {
            // The file grew after the metadata check. Never process a partial
            // file as if it were a complete search.
            truncated = true;
            break;
        }
        bytes_read += bytes.len() as u64;
        let content = match std::str::from_utf8(&bytes) {
            Ok(content) if !content.contains('\0') => content,
            _ => {
                skipped_binary += 1;
                continue;
            }
        };
        files_searched += 1;
        for (index, line) in content.lines().enumerate() {
            if let Some(position) = line.find(query) {
                if matches.len() == max_results {
                    truncated = true;
                    break 'search;
                }
                let mut start = position.saturating_sub(200);
                while !line.is_char_boundary(start) {
                    start += 1;
                }
                let mut end = (start + MAX_SEARCH_LINE_BYTES).min(line.len());
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                matches.push(json!({
                    "path": path, "line": index + 1,
                    "column": line[..position].chars().count() + 1,
                    "text": &line[start..end], "text_truncated": start > 0 || end < line.len()
                }));
            }
        }
    }
    Ok(json!({
        "path": relative, "query": query, "matches": matches,
        "files_searched": files_searched, "truncated": truncated,
        "skipped_files": {"binary": skipped_binary, "too_large": skipped_large, "unreadable": skipped_unreadable}
    }))
}

async fn workspace_write(arguments: Value, context: &ToolContext) -> Result<Value> {
    if !context.allow_writes {
        bail!(
            "workspace_write is disabled for environment '{}'; enable allow_writes",
            context.environment
        );
    }
    let raw_path = arguments
        .get("path")
        .and_then(Value::as_str)
        .context("workspace_write.path must be a string")?;
    let relative = relative_path(raw_path)?;
    let content = arguments
        .get("content")
        .and_then(Value::as_str)
        .context("workspace_write.content must be a string")?;
    if content.len() as u64 > MAX_FILE_BYTES {
        bail!("workspace_write content exceeds the 10 MiB limit");
    }
    let file = writable_workspace_path(context, &relative).await?;
    tokio::fs::write(&file, content).await?;
    Ok(json!({"path": relative, "bytes": content.len(), "written": true}))
}

fn relative_path(raw_path: &str) -> Result<PathBuf> {
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

async fn workspace_root(context: &ToolContext) -> Result<PathBuf> {
    let root = context
        .workspace
        .as_ref()
        .context("no workspace is configured for this environment")?;
    tokio::fs::canonicalize(root)
        .await
        .with_context(|| format!("workspace does not exist: {}", root.display()))
}

async fn existing_workspace_path(context: &ToolContext, relative: &Path) -> Result<PathBuf> {
    let root = workspace_root(context).await?;
    let candidate = tokio::fs::canonicalize(root.join(relative))
        .await
        .with_context(|| format!("workspace path does not exist: {}", relative.display()))?;
    if !candidate.starts_with(&root) {
        bail!("path escapes the configured workspace");
    }
    Ok(candidate)
}

/// Resolve a write target without ever touching the filesystem outside the
/// workspace: each parent directory is checked before the next one is
/// created, and the final path must not be a symbolic link.
async fn writable_workspace_path(context: &ToolContext, relative: &Path) -> Result<PathBuf> {
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
    use super::{register_builtin_tools, relative_path};
    use crate::tools::{ToolContext, ToolRegistry};
    use serde_json::json;

    fn context(workspace: &std::path::Path, allow_writes: bool) -> ToolContext {
        ToolContext {
            workspace: Some(workspace.to_path_buf()),
            allow_writes,
            ..ToolContext::default()
        }
    }

    fn registry() -> ToolRegistry {
        let registry = ToolRegistry::new();
        register_builtin_tools(&registry).unwrap();
        registry
    }

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

    #[tokio::test]
    async fn writes_and_reads_inside_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let registry = registry();
        let context = context(workspace.path(), true);

        registry
            .execute_with_context(
                "workspace_write",
                json!({"path": "nested/dir/note.txt", "content": "hello"}),
                &context,
            )
            .await
            .unwrap();
        let value = registry
            .execute_with_context(
                "workspace_read",
                json!({"path": "nested/dir/note.txt"}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(value["content"], "hello");
        assert_eq!(value["truncated"], false);
        assert!(value["next_offset"].is_null());
    }

    #[tokio::test]
    async fn read_pages_preserve_utf8_and_can_be_reassembled() {
        let workspace = tempfile::tempdir().unwrap();
        let content = "a日本語🙂z";
        std::fs::write(workspace.path().join("note.txt"), content).unwrap();
        let registry = registry();
        let context = context(workspace.path(), false);
        let mut offset = 0;
        let mut reassembled = String::new();
        loop {
            let page = registry
                .execute_with_context(
                    "workspace_read",
                    json!({"path": "note.txt", "offset": offset, "max_bytes": 4}),
                    &context,
                )
                .await
                .unwrap();
            let chunk = page["content"].as_str().unwrap();
            assert!(!chunk.is_empty());
            assert!(chunk.len() <= 4);
            assert_eq!(page["total_bytes"], content.len());
            reassembled.push_str(chunk);
            match page["next_offset"].as_u64() {
                Some(next) => {
                    assert!(next > offset);
                    offset = next;
                }
                None => break,
            }
        }
        assert_eq!(reassembled, content);
        for arguments in [
            json!({"path": "note.txt", "offset": 2}),
            json!({"path": "note.txt", "offset": 1000}),
            json!({"path": "note.txt", "offset": 1, "max_bytes": 1}),
        ] {
            assert!(registry
                .execute_with_context("workspace_read", arguments, &context)
                .await
                .is_err());
        }
        let end = registry
            .execute_with_context(
                "workspace_read",
                json!({"path": "note.txt", "offset": content.len()}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(end["content"], "");
        assert_eq!(end["truncated"], false);
    }

    #[tokio::test]
    async fn default_read_returns_a_bounded_page_for_large_files() {
        let workspace = tempfile::tempdir().unwrap();
        let content = "x".repeat(super::DEFAULT_READ_BYTES as usize + 10);
        std::fs::write(workspace.path().join("large.txt"), content).unwrap();
        let page = registry()
            .execute_with_context(
                "workspace_read",
                json!({"path": "large.txt"}),
                &context(workspace.path(), false),
            )
            .await
            .unwrap();
        assert_eq!(page["bytes"], super::DEFAULT_READ_BYTES);
        assert_eq!(page["truncated"], true);
        assert_eq!(page["next_offset"], super::DEFAULT_READ_BYTES);
    }

    #[tokio::test]
    async fn directory_pages_are_sorted_without_gaps_or_duplicates() {
        let workspace = tempfile::tempdir().unwrap();
        for name in ["z.txt", "a.txt", "middle.txt", "b.txt", "last.txt"] {
            std::fs::write(workspace.path().join(name), name).unwrap();
        }
        let registry = registry();
        let context = context(workspace.path(), false);
        let mut after = String::new();
        let mut names = Vec::new();
        loop {
            let page = registry
                .execute_with_context(
                    "workspace_list",
                    json!({"limit": 2, "after": after}),
                    &context,
                )
                .await
                .unwrap();
            names.extend(
                page["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entry| entry["name"].as_str().unwrap().to_owned()),
            );
            match page["next_after"].as_str() {
                Some(next) => {
                    assert_eq!(page["truncated"], true);
                    after = next.to_owned();
                }
                None => {
                    assert_eq!(page["truncated"], false);
                    break;
                }
            }
        }
        assert_eq!(names, ["a.txt", "b.txt", "last.txt", "middle.txt", "z.txt"]);
    }

    #[tokio::test]
    async fn optional_arguments_reject_wrong_types_and_out_of_range_values() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("note.txt"), "hello").unwrap();
        let registry = registry();
        let context = context(workspace.path(), false);
        for (tool, arguments) in [
            ("workspace_list", json!([])),
            ("workspace_list", json!({"path": 42})),
            ("workspace_list", json!({"after": null})),
            ("workspace_list", json!({"limit": 0})),
            ("workspace_list", json!({"limit": 1001})),
            ("workspace_read", json!({"path": "note.txt", "offset": -1})),
            (
                "workspace_read",
                json!({"path": "note.txt", "max_bytes": "4"}),
            ),
            (
                "workspace_read",
                json!({"path": "note.txt", "max_bytes": 0}),
            ),
            (
                "workspace_read",
                json!({"path": "note.txt", "max_bytes": super::MAX_FILE_BYTES + 1}),
            ),
            (
                "workspace_search",
                json!({"query": "hello", "max_results": false}),
            ),
            ("workspace_search", json!({"query": ""})),
            ("workspace_search", json!({"query": "hello\nworld"})),
        ] {
            assert!(
                registry
                    .execute_with_context(tool, arguments.clone(), &context)
                    .await
                    .is_err(),
                "{tool}: {arguments}"
            );
        }
    }

    #[tokio::test]
    async fn search_finds_lines_and_skips_binary_and_generated_files() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("src")).unwrap();
        std::fs::create_dir(workspace.path().join("target")).unwrap();
        std::fs::write(
            workspace.path().join("src/lib.rs"),
            "no match\n日本語 needle here\nneedle twice needle\n",
        )
        .unwrap();
        std::fs::write(workspace.path().join("target/output.txt"), "needle").unwrap();
        std::fs::write(workspace.path().join("binary.bin"), b"needle\0").unwrap();
        let registry = registry();
        let context = context(workspace.path(), false);
        let results = registry
            .execute_with_context("workspace_search", json!({"query": "needle"}), &context)
            .await
            .unwrap();
        let matches = results["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0]["line"], 2);
        assert_eq!(matches[0]["column"], 5);
        assert_eq!(matches[0]["text"], "日本語 needle here");
        assert_eq!(matches[1]["line"], 3);
        assert_eq!(results["files_searched"], 1);
        assert_eq!(results["skipped_files"]["binary"], 1);
        assert_eq!(results["truncated"], false);

        let limited = registry
            .execute_with_context(
                "workspace_search",
                json!({"query": "needle", "max_results": 1}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(limited["matches"].as_array().unwrap().len(), 1);
        assert_eq!(limited["truncated"], true);
        let scoped = registry
            .execute_with_context(
                "workspace_search",
                json!({"query": "needle", "path": "target/output.txt", "max_results": 1}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(scoped["matches"].as_array().unwrap().len(), 1);
        assert_eq!(scoped["truncated"], false);
    }

    #[tokio::test]
    async fn search_truncates_long_lines_around_the_match_at_utf8_boundaries() {
        let workspace = tempfile::tempdir().unwrap();
        let content = format!("{}needle{}", "日".repeat(2000), "本".repeat(2000));
        std::fs::write(workspace.path().join("long.txt"), content).unwrap();
        let results = registry()
            .execute_with_context(
                "workspace_search",
                json!({"query": "needle"}),
                &context(workspace.path(), false),
            )
            .await
            .unwrap();
        let found = &results["matches"][0];
        assert_eq!(found["column"], 2001);
        assert_eq!(found["text_truncated"], true);
        let text = found["text"].as_str().unwrap();
        assert!(text.contains("needle"));
        assert!(text.len() <= super::MAX_SEARCH_LINE_BYTES);
    }

    #[tokio::test]
    async fn search_reports_files_too_large_to_scan() {
        let workspace = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(workspace.path().join("large.txt")).unwrap();
        file.set_len(super::MAX_FILE_BYTES + 1).unwrap();
        let results = registry()
            .execute_with_context(
                "workspace_search",
                json!({"query": "needle"}),
                &context(workspace.path(), false),
            )
            .await
            .unwrap();
        assert_eq!(results["skipped_files"]["too_large"], 1);
        assert_eq!(results["files_searched"], 0);
    }

    #[tokio::test]
    async fn read_rejects_invalid_utf8_even_at_end_of_file() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("invalid.txt"), [0xe6, 0x97]).unwrap();
        assert!(registry()
            .execute_with_context(
                "workspace_read",
                json!({"path": "invalid.txt"}),
                &context(workspace.path(), false),
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn write_requires_allow_writes() {
        let workspace = tempfile::tempdir().unwrap();
        let result = registry()
            .execute_with_context(
                "workspace_write",
                json!({"path": "note.txt", "content": "hello"}),
                &context(workspace.path(), false),
            )
            .await;
        assert!(result.is_err());
        assert!(!workspace.path().join("note.txt").exists());
    }

    #[cfg(unix)]
    fn symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
        if target.is_dir() {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }

    #[tokio::test]
    async fn write_does_not_follow_symlinks_out_of_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("target.txt"), "original").unwrap();
        let links = symlink(outside.path(), &workspace.path().join("dir_link")).and_then(|_| {
            symlink(
                &outside.path().join("target.txt"),
                &workspace.path().join("file_link"),
            )
        });
        if let Err(error) = links {
            // Creating symlinks needs extra privileges on some Windows setups.
            eprintln!("skipping symlink test: {error}");
            return;
        }
        let registry = registry();
        let context = context(workspace.path(), true);

        let through_dir = registry
            .execute_with_context(
                "workspace_write",
                json!({"path": "dir_link/created/x.txt", "content": "x"}),
                &context,
            )
            .await;
        assert!(through_dir.is_err());
        assert!(!outside.path().join("created").exists());

        let through_file = registry
            .execute_with_context(
                "workspace_write",
                json!({"path": "file_link", "content": "overwritten"}),
                &context,
            )
            .await;
        assert!(through_file.is_err());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("target.txt")).unwrap(),
            "original"
        );

        for (tool, arguments) in [
            ("workspace_read", json!({"path": "file_link"})),
            ("workspace_list", json!({"path": "dir_link"})),
            (
                "workspace_search",
                json!({"path": "dir_link", "query": "original"}),
            ),
        ] {
            assert!(registry
                .execute_with_context(tool, arguments, &context)
                .await
                .is_err());
        }
        let search = registry
            .execute_with_context("workspace_search", json!({"query": "original"}), &context)
            .await
            .unwrap();
        assert!(search["matches"].as_array().unwrap().is_empty());
    }
}
