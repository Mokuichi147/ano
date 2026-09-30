//! Workspace file tools: list, read, search, edit, and write.
//!
//! These tools are confined to `ToolContext::workspace`. Paths must be
//! relative, every existing ancestor is canonicalized and checked against the
//! workspace root, and writes never follow a symbolic link.

use super::{
    args::{optional_bool, optional_integer, optional_string},
    glob::Glob,
    non_strict_definition,
    paths::{
        commit_workspace_file, existing_workspace_path, relative_path, workspace_root,
        writable_workspace_path,
    },
    walk::{relative_to, walk_workspace},
};
use crate::{
    application::registry::ToolRegistry,
    domain::tool::{ToolContext, ToolDefinition},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, ffi::OsString, path::Path, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub(super) const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const DEFAULT_READ_BYTES: u64 = 64 * 1024;
const MAX_LIST_ENTRIES: usize = 1000;
const MAX_SEARCH_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SEARCH_LINE_BYTES: usize = 2000;
const MAX_READ_LINES: u64 = 100_000;

/// Register the workspace file tools into `registry`.
pub(super) fn register(registry: &ToolRegistry) -> Result<()> {
    let mutations = Arc::new(tokio::sync::Mutex::new(()));
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
            "Read a UTF-8 text file in bounded pages (64 KiB by default). Continue from next_offset when truncated; offsets count bytes. Alternatively pass start_line (1-based, e.g. a line number from workspace_search) and optionally max_lines to read whole lines; continue from next_line. Line reads always return the full-file sha256.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Relative file path"},
                    "offset": {"type": "integer", "minimum": 0, "description": "Byte offset from a previous next_offset; defaults to 0"},
                    "start_line": {"type": "integer", "minimum": 1, "description": "First line to read (1-based); cannot be combined with offset"},
                    "max_lines": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LINES, "description": "Lines to read from start_line; the page also stops at max_bytes"},
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
            "Find text in workspace UTF-8 files and return paths, line numbers, and excerpts. Literal and case-sensitive by default; set regex=true for a regular expression (Rust syntax) or ignore_case=true. include limits files by a glob such as *.rs or src/**/*.ts. Searches recursively, skips symlinks, .git/target/node_modules/.venv directories, and entries matched by .gitignore (unless include_ignored=true), and limits work to 10000 entries and 32 MiB. Narrow path or include if truncated.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1, "description": "Text or regular expression matched within one line"},
                    "path": {"type": "string", "description": "Relative file or directory path; defaults to ."},
                    "regex": {"type": "boolean", "description": "Treat query as a regular expression; defaults to false"},
                    "ignore_case": {"type": "boolean", "description": "Match case-insensitively; defaults to false"},
                    "include": {"type": "string", "description": "Only search files matching this glob, relative to path"},
                    "include_ignored": {"type": "boolean", "description": "Also search entries matched by .gitignore; defaults to false"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Matching lines to return; defaults to 100"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        |arguments, context| async move { workspace_search(arguments, &context).await },
    )?;
    registry.register_contextual(
        non_strict_definition(
            "workspace_find",
            "Find workspace files or directories by path glob. A pattern without / matches names at any depth (*.rs, Cargo.toml); otherwise it matches the path relative to path (src/**/*.rs). Supports *, ?, ** and {a,b}. Skips symlinks, .git/target/node_modules/.venv directories, and entries matched by .gitignore unless include_ignored=true.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "minLength": 1, "description": "Glob pattern"},
                    "path": {"type": "string", "description": "Relative directory to search; defaults to ."},
                    "kind": {"type": "string", "enum": ["file", "directory", "any"], "description": "Entries to return; defaults to file"},
                    "include_ignored": {"type": "boolean", "description": "Also return entries matched by .gitignore; defaults to false"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Defaults to 200"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        ),
        |arguments, context| async move { workspace_find(arguments, &context).await },
    )?;
    registry.register_contextual(
        non_strict_definition(
            "workspace_edit",
            "Apply exact text replacements to one workspace file. Read it first. Every old_text must match exactly once; all edits are validated before saving. Optional expected_sha256 detects stale reads; dry_run previews without writing.",
            json!({
                "type":"object", "properties": {
                    "path":{"type":"string"},
                    "edits":{"type":"array","minItems":1,"maxItems":100,"items":{
                        "type":"object","properties":{"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"}},
                        "required":["old_text","new_text"],"additionalProperties":false}},
                    "expected_sha256":{"type":"string","description":"Full-file SHA-256 returned by a complete workspace_read or workspace_edit"},
                    "dry_run":{"type":"boolean","description":"Preview edits without saving; defaults to false"}
                }, "required":["path","edits"],"additionalProperties":false
            }),
        ),
        {
            let mutations = Arc::clone(&mutations);
            move |arguments, context| {
                let mutations = Arc::clone(&mutations);
                async move {
                    let guard = mutations.lock_owned().await;
                    workspace_edit(arguments, &context, guard).await
                }
            }
        },
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
        {
            let mutations = Arc::clone(&mutations);
            move |arguments, context| {
                let mutations = Arc::clone(&mutations);
                async move {
                    let guard = mutations.lock_owned().await;
                    workspace_write(arguments, &context, guard).await
                }
            }
        },
    )?;
    // Moves, deletes, and commits share the lock, so they never race an edit.
    super::manage::register(registry, Arc::clone(&mutations))?;
    super::git::register(registry, mutations)?;
    Ok(())
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
    let file = existing_workspace_path(context, &relative).await?;
    let metadata = tokio::fs::metadata(&file).await?;
    if !metadata.is_file() {
        bail!("workspace path is not a file: {}", relative.display());
    }
    if arguments
        .get("start_line")
        .is_some_and(|value| !value.is_null())
    {
        if arguments
            .get("offset")
            .is_some_and(|value| !value.is_null())
        {
            bail!("pass either offset or start_line, not both");
        }
        return read_lines(&arguments, &relative, &file, metadata.len(), max_bytes).await;
    }
    let offset = optional_integer(&arguments, "offset", 0, 0, u64::MAX)?;
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
    let sha256 = (offset == 0 && !truncated).then(|| digest(content.as_bytes()));
    Ok(json!({
        "path": relative, "bytes": content.len(), "content": content,
        "offset": offset, "total_bytes": metadata.len(), "truncated": truncated,
        "next_offset": next_offset, "sha256": sha256
    }))
}

/// `workspace_read` by line numbers. The whole file is read, so the result
/// can report the line count and the full-file hash.
async fn read_lines(
    arguments: &Value,
    relative: &Path,
    file: &Path,
    total_bytes: u64,
    max_bytes: u64,
) -> Result<Value> {
    let start_line = optional_integer(arguments, "start_line", 1, 1, u64::MAX)?;
    let max_lines = optional_integer(arguments, "max_lines", MAX_READ_LINES, 1, MAX_READ_LINES)?;
    if total_bytes > MAX_FILE_BYTES {
        bail!("file is larger than {MAX_FILE_BYTES} bytes; read it by offset instead");
    }
    let text = String::from_utf8(tokio::fs::read(file).await?)
        .context("workspace file is not valid UTF-8")?;
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    let total_lines = lines.len() as u64;
    if start_line > total_lines.max(1) {
        bail!("start_line exceeds the file's {total_lines} lines");
    }
    let mut content = String::new();
    let mut end_line = start_line - 1;
    for line in lines
        .iter()
        .skip(start_line as usize - 1)
        .take(max_lines as usize)
    {
        if (content.len() + line.len()) as u64 > max_bytes {
            if content.is_empty() {
                bail!("line {start_line} is longer than max_bytes; read it by offset instead");
            }
            break;
        }
        content.push_str(line);
        end_line += 1;
    }
    let truncated = end_line < total_lines;
    Ok(json!({
        "path": relative, "content": content, "start_line": start_line,
        "end_line": end_line, "total_lines": total_lines, "truncated": truncated,
        "next_line": truncated.then_some(end_line + 1), "total_bytes": total_bytes,
        "sha256": digest(text.as_bytes())
    }))
}

/// How `workspace_search` finds a query in a line.
enum LineMatcher {
    Literal(String),
    Regex(regex::Regex),
}

impl LineMatcher {
    fn new(query: &str, is_regex: bool, ignore_case: bool) -> Result<Self> {
        if !is_regex && !ignore_case {
            return Ok(Self::Literal(query.to_string()));
        }
        let pattern = if is_regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        let regex = regex::RegexBuilder::new(&pattern)
            .case_insensitive(ignore_case)
            .size_limit(1 << 20)
            .build()
            .context("workspace_search.query is not a valid regular expression")?;
        Ok(Self::Regex(regex))
    }

    /// Byte position of the first match in `line`.
    fn find(&self, line: &str) -> Option<usize> {
        match self {
            Self::Literal(query) => line.find(query.as_str()),
            Self::Regex(regex) => regex.find(line).map(|found| found.start()),
        }
    }
}

async fn workspace_search(arguments: Value, context: &ToolContext) -> Result<Value> {
    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .context("workspace_search.query must be a string")?;
    if query.is_empty() || query.contains(['\r', '\n']) {
        bail!("workspace_search.query must be non-empty text on one line");
    }
    let is_regex = optional_bool(&arguments, "regex")?;
    let ignore_case = optional_bool(&arguments, "ignore_case")?;
    let matcher = LineMatcher::new(query, is_regex, ignore_case)?;
    let include = match arguments.get("include") {
        None | Some(Value::Null) => None,
        Some(value) => Some(Glob::new(
            value.as_str().context("include must be a glob string")?,
        )?),
    };
    let relative = relative_path(optional_string(&arguments, "path", ".")?)?;
    let max_results = optional_integer(&arguments, "max_results", 100, 1, 1000)? as usize;
    let root = workspace_root(context).await?;
    existing_workspace_path(context, &relative).await?;
    let include_ignored = optional_bool(&arguments, "include_ignored")?;
    let walk = walk_workspace(context, &root, &relative, !include_ignored).await;
    let mut matches = Vec::new();
    let mut files_searched = 0;
    let mut bytes_read = 0;
    let mut skipped_binary = 0;
    let mut skipped_large = 0;
    let mut skipped_unreadable = walk.unreadable;
    let mut truncated = walk.truncated;

    'search: for entry in walk.entries.iter().filter(|entry| !entry.is_dir) {
        if include
            .as_ref()
            .is_some_and(|glob| !glob.matches(&relative_to(&relative, &entry.path)))
        {
            continue;
        }
        if entry.bytes > MAX_FILE_BYTES {
            skipped_large += 1;
            continue;
        }
        if entry.bytes > MAX_SEARCH_BYTES - bytes_read {
            truncated = true;
            break;
        }
        let file = match tokio::fs::File::open(&entry.absolute).await {
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
            if let Some(position) = matcher.find(line) {
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
                    "path": entry.path, "line": index + 1,
                    "column": line[..position].chars().count() + 1,
                    "text": &line[start..end], "text_truncated": start > 0 || end < line.len()
                }));
            }
        }
    }
    Ok(json!({
        "path": relative, "query": query, "matches": matches,
        "files_searched": files_searched, "truncated": truncated,
        "skipped_files": {"binary": skipped_binary, "too_large": skipped_large, "unreadable": skipped_unreadable, "ignored": walk.ignored}
    }))
}

async fn workspace_find(arguments: Value, context: &ToolContext) -> Result<Value> {
    let pattern = arguments
        .get("pattern")
        .and_then(Value::as_str)
        .context("workspace_find.pattern must be a string")?;
    let glob = Glob::new(pattern)?;
    let kind = optional_string(&arguments, "kind", "file")?;
    if !matches!(kind, "file" | "directory" | "any") {
        bail!("workspace_find.kind must be file, directory, or any");
    }
    let relative = relative_path(optional_string(&arguments, "path", ".")?)?;
    let max_results = optional_integer(&arguments, "max_results", 200, 1, 1000)? as usize;
    let root = workspace_root(context).await?;
    if !tokio::fs::metadata(existing_workspace_path(context, &relative).await?)
        .await?
        .is_dir()
    {
        bail!("workspace path is not a directory: {}", relative.display());
    }
    let include_ignored = optional_bool(&arguments, "include_ignored")?;
    let walk = walk_workspace(context, &root, &relative, !include_ignored).await;
    let mut truncated = walk.truncated;
    let mut results = Vec::new();
    for entry in &walk.entries {
        let wanted = match kind {
            "file" => !entry.is_dir,
            "directory" => entry.is_dir,
            _ => true,
        };
        if !wanted || !glob.matches(&relative_to(&relative, &entry.path)) {
            continue;
        }
        if results.len() == max_results {
            truncated = true;
            break;
        }
        results.push(json!({
            "path": entry.path,
            "kind": if entry.is_dir { "directory" } else { "file" },
            "bytes": (!entry.is_dir).then_some(entry.bytes),
        }));
    }
    Ok(json!({
        "path": relative, "pattern": pattern, "matches": results,
        "truncated": truncated, "skipped_unreadable": walk.unreadable, "skipped_ignored": walk.ignored
    }))
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

async fn workspace_edit(
    arguments: Value,
    context: &ToolContext,
    guard: tokio::sync::OwnedMutexGuard<()>,
) -> Result<Value> {
    if !context.allow_writes {
        bail!("workspace_edit requires allow_writes for this environment");
    }
    let relative = relative_path(
        arguments["path"]
            .as_str()
            .context("workspace_edit.path must be a string")?,
    )?;
    let edits = arguments["edits"]
        .as_array()
        .context("workspace_edit.edits must be an array")?;
    if edits.is_empty() || edits.len() > 100 {
        bail!("workspace_edit requires 1 to 100 edits");
    }
    let dry_run = match arguments.get("dry_run") {
        None => false,
        Some(value) => value.as_bool().context("dry_run must be a boolean")?,
    };
    // Resolve for reading before the write resolver, so an invalid edit never
    // creates directories as a side effect.
    let existing = existing_workspace_path(context, &relative).await?;
    let metadata = tokio::fs::metadata(&existing).await?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        bail!("workspace_edit requires a regular UTF-8 file up to 10 MiB");
    }
    let file = writable_workspace_path(context, &relative).await?;
    let mut bytes = Vec::new();
    tokio::fs::File::open(&file)
        .await?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("workspace_edit file exceeds 10 MiB");
    }
    let before_hash = digest(&bytes);
    if let Some(expected) = arguments.get("expected_sha256") {
        let expected = expected
            .as_str()
            .context("expected_sha256 must be a string")?;
        if expected.len() != 64 || !expected.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("expected_sha256 must contain 64 hexadecimal characters");
        }
        if !expected.eq_ignore_ascii_case(&before_hash) {
            bail!("edit conflict: file changed since it was read; read it again");
        }
    }
    let mut content =
        String::from_utf8(bytes.clone()).context("workspace file is not valid UTF-8")?;
    let mut preview = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let old = edit["old_text"]
            .as_str()
            .filter(|old| !old.is_empty())
            .context("old_text must be a non-empty string")?;
        let new = edit["new_text"]
            .as_str()
            .context("new_text must be a string")?;
        // Check overlapping occurrences too, e.g. 'aa' in 'aaa'.
        let Some(position) = content.find(old) else {
            bail!(
                "edit {}: old_text was not found; read the file again",
                index + 1
            )
        };
        let following = position + content[position..].chars().next().unwrap().len_utf8();
        if content[following..].contains(old) {
            bail!(
                "edit {}: old_text matches more than once; include more surrounding context",
                index + 1
            );
        }
        let size = content.len() - old.len() + new.len();
        if size as u64 > MAX_FILE_BYTES {
            bail!("edited file would exceed 10 MiB");
        }
        preview.push(json!({"line":content[..position].bytes().filter(|b| *b == b'\n').count() + 1,
            "old_text": old.chars().take(500).collect::<String>(), "new_text": new.chars().take(500).collect::<String>(),
            "preview_truncated":old.chars().count() > 500 || new.chars().count() > 500}));
        content.replace_range(position..position + old.len(), new);
    }
    let after_hash = digest(content.as_bytes());
    let changed = before_hash != after_hash;
    let size = content.len();
    if !dry_run && changed {
        commit_workspace_file(
            file,
            workspace_root(context).await?,
            content.into_bytes(),
            Some(bytes),
            Some(metadata.permissions()),
            guard,
        )
        .await?;
    }
    Ok(
        json!({"path":relative, "changed":changed,"written":!dry_run && changed,"dry_run":dry_run,
        "bytes":size,"before_sha256":before_hash,"after_sha256":after_hash,
        "sha256":if dry_run { &before_hash } else { &after_hash },"edits":preview}),
    )
}

async fn workspace_write(
    arguments: Value,
    context: &ToolContext,
    guard: tokio::sync::OwnedMutexGuard<()>,
) -> Result<Value> {
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
    let permissions = tokio::fs::metadata(&file)
        .await
        .ok()
        .map(|metadata| metadata.permissions());
    commit_workspace_file(
        file,
        workspace_root(context).await?,
        content.as_bytes().to_vec(),
        None,
        permissions,
        guard,
    )
    .await?;
    Ok(
        json!({"path": relative, "bytes": content.len(), "written": true, "sha256":digest(content.as_bytes())}),
    )
}

#[cfg(test)]
mod tests {
    use crate::{
        application::registry::ToolRegistry, domain::tool::ToolContext,
        infrastructure::tools::register_builtin_tools,
    };
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

    #[tokio::test]
    async fn edit_previews_then_applies_exact_changes_with_hash_check() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("note.txt");
        std::fs::write(&path, "first\r\n日本語\r\nlast\r\n").unwrap();
        let registry = registry();
        let context = context(workspace.path(), true);
        let read = registry
            .execute_with_context("workspace_read", json!({"path":"note.txt"}), &context)
            .await
            .unwrap();
        let mut edit = json!({"path":"note.txt","expected_sha256":read["sha256"],"dry_run":true,
            "edits":[{"old_text":"日本語","new_text":"修正済み"}]});
        let preview = registry
            .execute_with_context("workspace_edit", edit.clone(), &context)
            .await
            .unwrap();
        assert_eq!(preview["written"], false);
        assert_eq!(preview["edits"][0]["line"], 2);
        assert!(std::fs::read_to_string(&path).unwrap().contains("日本語"));
        edit["dry_run"] = json!(false);
        let written = registry
            .execute_with_context("workspace_edit", edit.clone(), &context)
            .await
            .unwrap();
        assert_eq!(written["written"], true);
        assert_eq!(written["sha256"], preview["after_sha256"]);
        assert_eq!(preview["sha256"], read["sha256"]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "first\r\n修正済み\r\nlast\r\n"
        );
        assert!(registry
            .execute_with_context("workspace_edit", edit, &context)
            .await
            .unwrap_err()
            .to_string()
            .contains("conflict"));
    }

    #[tokio::test]
    async fn invalid_or_ambiguous_edits_leave_file_unchanged() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("note.txt");
        std::fs::write(&path, "aaa\nunique\n").unwrap();
        let registry = registry();
        let context = context(workspace.path(), true);
        for edits in [
            json!([{"old_text":"aa","new_text":"x"}]),
            json!([{"old_text":"unique","new_text":"changed"},{"old_text":"missing","new_text":"x"}]),
            json!([{"old_text":"","new_text":"x"}]),
            json!([]),
        ] {
            assert!(registry
                .execute_with_context(
                    "workspace_edit",
                    json!({"path":"note.txt","edits":edits}),
                    &context
                )
                .await
                .is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaa\nunique\n");
        }
        let mut read_only = context.clone();
        read_only.allow_writes = false;
        assert!(registry.execute_with_context("workspace_edit", json!({"path":"note.txt","dry_run":true,"edits":[{"old_text":"unique","new_text":"x"}]}), &read_only).await.is_err());
    }

    #[tokio::test]
    async fn concurrent_edits_reject_stale_content_instead_of_losing_changes() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("note.txt");
        std::fs::write(&path, "original").unwrap();
        let registry = registry();
        let context = context(workspace.path(), true);
        let args = |replacement| json!({"path":"note.txt","edits":[{"old_text":"original","new_text":replacement}]});
        let (first, second) = tokio::join!(
            registry.execute_with_context("workspace_edit", args("one"), &context),
            registry.execute_with_context("workspace_edit", args("two"), &context)
        );
        assert_ne!(first.is_ok(), second.is_ok());
        assert!(matches!(
            std::fs::read_to_string(&path).unwrap().as_str(),
            "one" | "two"
        ));
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
    async fn line_reads_page_through_whole_lines_with_the_file_hash() {
        let workspace = tempfile::tempdir().unwrap();
        let content = "one\n二\nthree\nfour";
        std::fs::write(workspace.path().join("note.txt"), content).unwrap();
        let registry = registry();
        let context = context(workspace.path(), false);
        let read = |arguments: serde_json::Value| {
            let registry = registry.clone();
            let context = context.clone();
            async move {
                registry
                    .execute_with_context("workspace_read", arguments, &context)
                    .await
            }
        };

        let page = read(json!({"path": "note.txt", "start_line": 2, "max_lines": 2}))
            .await
            .unwrap();
        assert_eq!(page["content"], "二\nthree\n");
        assert_eq!(page["start_line"], 2);
        assert_eq!(page["end_line"], 3);
        assert_eq!(page["total_lines"], 4);
        assert_eq!(page["next_line"], 4);
        assert_eq!(page["sha256"], super::digest(content.as_bytes()));

        let last = read(json!({"path": "note.txt", "start_line": 4}))
            .await
            .unwrap();
        assert_eq!(last["content"], "four");
        assert_eq!(last["truncated"], false);
        assert!(last["next_line"].is_null());

        // The byte budget ends a page at a line boundary.
        let bounded = read(json!({"path": "note.txt", "start_line": 1, "max_bytes": 9}))
            .await
            .unwrap();
        assert_eq!(bounded["content"], "one\n二\n");
        assert_eq!(bounded["next_line"], 3);

        for arguments in [
            json!({"path": "note.txt", "start_line": 5}),
            json!({"path": "note.txt", "start_line": 0}),
            json!({"path": "note.txt", "start_line": 1, "offset": 0}),
            json!({"path": "note.txt", "start_line": 3, "max_bytes": 2}),
        ] {
            assert!(read(arguments.clone()).await.is_err(), "{arguments}");
        }
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
    async fn search_supports_regex_case_folding_and_file_globs() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("src")).unwrap();
        std::fs::write(
            workspace.path().join("src/lib.rs"),
            "fn parse_config() {}\nfn Parse() {}\n",
        )
        .unwrap();
        std::fs::write(workspace.path().join("notes.md"), "parse_config docs\n").unwrap();
        let registry = registry();
        let context = context(workspace.path(), false);
        let search = |arguments| {
            let registry = registry.clone();
            let context = context.clone();
            async move {
                registry
                    .execute_with_context("workspace_search", arguments, &context)
                    .await
            }
        };

        let regex = search(json!({"query": r"fn \w+_config\(", "regex": true}))
            .await
            .unwrap();
        assert_eq!(regex["matches"].as_array().unwrap().len(), 1);
        assert_eq!(regex["matches"][0]["path"], "./src/lib.rs");

        let folded = search(json!({"query": "PARSE", "ignore_case": true, "include": "*.rs"}))
            .await
            .unwrap();
        let lines = folded["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|found| found["line"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(lines, vec![1, 2]);

        // Regex metacharacters stay literal unless regex=true.
        let literal = search(json!({"query": "config()", "ignore_case": true}))
            .await
            .unwrap();
        assert_eq!(literal["matches"].as_array().unwrap().len(), 1);
        assert!(search(json!({"query": "(", "regex": true})).await.is_err());
    }

    #[tokio::test]
    async fn search_and_find_honor_gitignore_files() {
        let workspace = tempfile::tempdir().unwrap();
        for path in ["dist", "src/gen", "logs", ".git/info"] {
            std::fs::create_dir_all(workspace.path().join(path)).unwrap();
        }
        for (path, content) in [
            (".gitignore", "dist/\n*.log\n!keep.log\n"),
            ("src/.gitignore", "gen/\n"),
            (".git/info/exclude", "local.txt\n"),
            ("dist/bundle.js", "needle"),
            ("src/gen/out.rs", "needle"),
            ("src/lib.rs", "needle"),
            ("logs/app.log", "needle"),
            ("logs/keep.log", "needle"),
            ("local.txt", "needle"),
            ("notes.txt", "needle"),
        ] {
            std::fs::write(workspace.path().join(path), content).unwrap();
        }
        let registry = registry();
        let context = context(workspace.path(), false);
        let run = |name: &'static str, arguments| {
            let registry = registry.clone();
            let context = context.clone();
            async move {
                let result = registry
                    .execute_with_context(name, arguments, &context)
                    .await
                    .unwrap();
                let mut paths = result["matches"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|found| found["path"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>();
                paths.sort();
                (paths, result)
            }
        };

        let (paths, result) = run("workspace_search", json!({"query": "needle"})).await;
        assert_eq!(paths, ["./logs/keep.log", "./notes.txt", "./src/lib.rs"]);
        assert_eq!(result["skipped_files"]["ignored"], 4);
        // Rules of the parent directories apply when a subdirectory is searched.
        let (paths, _) = run(
            "workspace_search",
            json!({"query": "needle", "path": "src"}),
        )
        .await;
        assert_eq!(paths, ["src/lib.rs"]);
        // An ignored directory named explicitly is searched.
        let (paths, _) = run(
            "workspace_search",
            json!({"query": "needle", "path": "dist"}),
        )
        .await;
        assert_eq!(paths, ["dist/bundle.js"]);
        let (paths, _) = run(
            "workspace_search",
            json!({"query": "needle", "include_ignored": true}),
        )
        .await;
        assert_eq!(paths.len(), 7);
        let (paths, result) = run("workspace_find", json!({"pattern": "*.{js,rs,log}"})).await;
        assert_eq!(paths, ["./logs/keep.log", "./src/lib.rs"]);
        assert_eq!(result["skipped_ignored"], 4);
    }

    #[tokio::test]
    async fn find_matches_globs_by_kind_and_skips_generated_directories() {
        let workspace = tempfile::tempdir().unwrap();
        for path in ["src/domain", "target/debug", "docs"] {
            std::fs::create_dir_all(workspace.path().join(path)).unwrap();
        }
        for path in [
            "Cargo.toml",
            "src/lib.rs",
            "src/domain/plan.rs",
            "target/debug/build.rs",
            "docs/guide.md",
        ] {
            std::fs::write(workspace.path().join(path), "x").unwrap();
        }
        let registry = registry();
        let context = context(workspace.path(), false);
        let find = |arguments| {
            let registry = registry.clone();
            let context = context.clone();
            async move {
                let result = registry
                    .execute_with_context("workspace_find", arguments, &context)
                    .await
                    .unwrap();
                result["matches"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|found| found["path"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            }
        };

        assert_eq!(
            find(json!({"pattern": "*.rs"})).await,
            vec!["./src/domain/plan.rs", "./src/lib.rs"]
        );
        assert_eq!(
            find(json!({"pattern": "*.rs", "path": "src/domain"})).await,
            vec!["src/domain/plan.rs"]
        );
        assert_eq!(
            find(json!({"pattern": "*.{toml,md}"})).await,
            vec!["./Cargo.toml", "./docs/guide.md"]
        );
        assert_eq!(
            find(json!({"pattern": "src/*", "kind": "directory"})).await,
            vec!["./src/domain"]
        );
        let limited = registry
            .execute_with_context(
                "workspace_find",
                json!({"pattern": "*", "kind": "any", "max_results": 2}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(limited["truncated"], true);
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

    #[tokio::test]
    async fn repository_metadata_is_never_written() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join(".git/hooks")).unwrap();
        std::fs::write(workspace.path().join(".git/config"), "[core]\n").unwrap();
        let registry = registry();
        let context = context(workspace.path(), true);
        for path in [".git/hooks/pre-commit", ".GIT/config", "sub/.git/config"] {
            let error = registry
                .execute_with_context(
                    "workspace_write",
                    json!({"path": path, "content": "#!/bin/sh\n"}),
                    &context,
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains(".git"), "{path}: {error}");
        }
        let edit = registry
            .execute_with_context(
                "workspace_edit",
                json!({"path": ".git/config", "edits": [{"old_text": "[core]", "new_text": "[core]\nfsmonitor = evil"}]}),
                &context,
            )
            .await;
        assert!(edit.is_err());
        assert!(!workspace.path().join(".git/hooks/pre-commit").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.path().join(".git/config")).unwrap(),
            "[core]\n"
        );
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
