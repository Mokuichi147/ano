//! `git_diff` and `git_commit_push`: show the uncommitted changes of the
//! workspace, and commit files and push them on a branch.
//!
//! GitHub itself is reached through MCP (such as the GitHub MCP server),
//! which cannot see the local checkout. This tool fills that gap without
//! `workspace_exec`: it commits only the listed files, never onto the
//! remote's default branch, and every call goes through approval because a
//! push publishes the files. It runs the `git` program, so pushes use the
//! user's own remotes, credential helpers, and SSH keys, but never the
//! repository's hooks: they are files the agent can write, and running them
//! would turn `allow_writes` into command execution.

use super::paths::{inside_git_dir, relative_path, workspace_root};
use crate::{
    application::registry::ToolRegistry,
    domain::{
        github::RepoRef,
        tool::{ToolContext, ToolDefinition},
    },
    harness::names::{GIT_COMMIT_PUSH_NAME, GIT_DIFF_NAME},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};
use tokio::{io::AsyncReadExt, process::Command, sync::Mutex};

const MAX_FILES: usize = 1000;
/// Diff text returned by `git_diff`; files beyond it are listed only.
const MAX_DIFF_BYTES: usize = 256 * 1024;

/// Register the tools. `mutations` is the lock of the workspace file tools,
/// so a commit never races an edit of the files it commits.
pub(super) fn register(registry: &ToolRegistry, mutations: Arc<Mutex<()>>) -> Result<()> {
    let mut diff = ToolDefinition::new(
        GIT_DIFF_NAME,
        "Show the uncommitted changes of the workspace against the last commit, including new untracked files: each changed file with its status and a sha256 of its state (content and executable bit), and the diff text (diff_truncated when it is cut or leaves out large files). Pass paths to limit it to those files (unchanged ones are listed with status unchanged). Read-only.",
        json!({
            "type": "object",
            "properties": {
                "paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_FILES, "description": "Workspace-relative paths to show; defaults to every changed file"}
            },
            "additionalProperties": false
        }),
    );
    // paths is optional.
    diff.strict = false;
    registry.register_contextual(diff, |arguments, context| async move {
        git_diff(arguments, &context).await
    })?;
    let mut definition = ToolDefinition::new(
        GIT_COMMIT_PUSH_NAME,
        "Commit the listed workspace files (added, modified, or deleted) with a message and push the branch to the git remote, for example before opening a pull request with a GitHub tool. Only the listed files are committed; repository hooks do not run. It never commits to the remote's default branch: when the workspace is on it, pass branch (such as ano/issue-6) to create that new branch from the current commit; uncommitted changes carry over to it. On another branch, later calls add commits to it. If a push failed, call it again on the same branch to retry. The files must be in the state the last review_changes saw; after any change, review again. Needs writes to be allowed, and each call needs approval. Returns the branch, the remote's default branch (the usual base of a pull request), the repository as owner/name with its host, and the commit.",
        json!({
            "type": "object",
            "properties": {
                "files": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_FILES, "description": "Workspace-relative paths of the files to commit"},
                "message": {"type": "string", "minLength": 1, "description": "Commit message"},
                "branch": {"type": "string", "description": "Branch to commit on: the checked-out branch, or a new branch that does not exist locally or on the remote. Defaults to the checked-out branch unless that is the default branch"}
            },
            "required": ["files", "message"],
            "additionalProperties": false
        }),
    )
    .with_approval()
    .available_when(|context| context.allow_writes);
    // branch is optional.
    definition.strict = false;
    registry.register_contextual(definition, move |arguments, context| {
        let mutations = Arc::clone(&mutations);
        async move {
            let _guard = mutations.lock_owned().await;
            git_commit_push(arguments, &context).await
        }
    })
}

async fn git_commit_push(arguments: Value, context: &ToolContext) -> Result<Value> {
    if !context.allow_writes {
        bail!("writes are not enabled for this environment (allow_writes)");
    }
    let files = paths(&arguments["files"], "files")?;
    let message = arguments["message"]
        .as_str()
        .filter(|message| !message.trim().is_empty())
        .context("message must be a non-empty string")?;
    let requested = match arguments.get("branch") {
        None | Some(Value::Null) => None,
        Some(branch) => Some(
            branch
                .as_str()
                .filter(|branch| !branch.trim().is_empty())
                .context("branch must be a non-empty string")?
                .trim(),
        ),
    };

    let root = workspace_root(context).await?;
    // Under the workspace lock, so no edit lands between this check and the
    // commit.
    check_reviewed(&root, &files, &arguments["reviewed"]).await?;
    let git = Git::open(&root).await?;
    let (remote, url) = git
        .push_remote()
        .await?
        .context("the repository has no git remote to push to")?;
    // Checks ask the repository the push goes to, which a `pushurl` can make
    // differ from the one fetched from.
    let target = git.push_target(&remote).await?;
    // Without the default branch, committing to it cannot be ruled out.
    let default_branch = git.default_branch(&target).await.with_context(|| {
        format!("could not determine the default branch of {remote}; check that the remote is reachable")
    })?;
    let current = git.current_branch().await?;
    let changed = git.has_changes(&files).await?;
    let (branch, created) = match (requested, current.as_deref()) {
        (Some(branch), _) if branch == default_branch => {
            bail!("'{branch}' is the default branch of {remote}; commit on another branch")
        }
        (Some(branch), current) if Some(branch) == current => (branch.to_string(), false),
        (Some(branch), _) => {
            if !changed {
                bail!("the listed files have no uncommitted changes");
            }
            if git.branch_exists(branch).await? {
                bail!(
                    "branch '{branch}' already exists but is not checked out; choose another name"
                );
            }
            if git.remote_branch_exists(&target, branch).await? {
                bail!("branch '{branch}' already exists on {remote}; choose another name");
            }
            git.create_branch(branch).await?;
            (branch.to_string(), true)
        }
        (None, Some(current)) if current != default_branch => (current.to_string(), false),
        (None, current) => bail!(
            "the workspace is on {}; pass branch to commit on a new branch",
            match current {
                Some(current) => format!("the default branch '{current}'"),
                None => "a detached HEAD".into(),
            }
        ),
    };
    let (commit, committed) = if changed {
        (git.commit_paths(message, &files).await?, true)
    } else if git.unpushed(&remote, &branch, &default_branch).await? {
        // A retry after a failed push: push what is already committed.
        (git.head_commit().await?, false)
    } else {
        bail!("the listed files have no uncommitted changes, and '{branch}' has nothing to push");
    };
    git.push(&remote, &branch).await.with_context(|| {
        format!("committed {commit} on '{branch}', but pushing it to {remote} failed; call git_commit_push again on this branch to retry the push")
    })?;
    let repository = RepoRef::from_remote_url(&url);
    Ok(json!({
        "branch": branch,
        "created_branch": created,
        "commit": commit,
        "committed": committed,
        "remote": remote,
        "default_branch": default_branch,
        "repository": repository.as_ref().map(ToString::to_string),
        "host": repository.map(|repository| repository.host),
        "note": format!("The workspace is now on branch '{branch}'."),
    }))
}

/// Workspace-relative file paths from a JSON array argument.
fn paths(value: &Value, name: &str) -> Result<Vec<String>> {
    value
        .as_array()
        .filter(|paths| !paths.is_empty() && paths.len() <= MAX_FILES)
        .with_context(|| format!("{name} must list 1 to {MAX_FILES} paths"))?
        .iter()
        .map(|path| {
            let path = path
                .as_str()
                .with_context(|| format!("{name} must be strings"))?;
            let relative = relative_path(path)?;
            if inside_git_dir(&relative) {
                bail!("files inside .git cannot be used");
            }
            // `./a` and `a//b` name the files that `git status` lists as `a`
            // and `a/b`, which reviews are recorded under.
            let normal: Vec<String> = relative
                .components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect();
            if normal.is_empty() {
                bail!("{name} must name files inside the workspace");
            }
            Ok(normal.join("/"))
        })
        .collect()
}

async fn git_diff(arguments: Value, context: &ToolContext) -> Result<Value> {
    let requested = match arguments.get("paths") {
        None | Some(Value::Null) => None,
        Some(value) => Some(paths(value, "paths")?),
    };
    let root = workspace_root(context).await?;
    let git = Git::open(&root).await?;
    let changed = git.changed_paths(requested.as_deref()).await?;
    let listed: Vec<String> = match &requested {
        Some(requested) => requested.clone(),
        None => changed.iter().map(|change| change.path.clone()).collect(),
    };
    let mut files = Vec::new();
    for path in listed {
        let status = changed
            .iter()
            .find(|change| change.path == path)
            .map_or("unchanged", |change| change.status);
        files.push(json!({
            "path": path,
            "status": status,
            "sha256": content_hash(&root.join(&path)).await?,
        }));
    }
    let (diff, truncated) = git.diff(&changed).await?;
    Ok(json!({ "files": files, "diff": diff, "diff_truncated": truncated }))
}

/// A sha256 of a file's state as git records it: its content (the target,
/// for a symbolic link) and its executable bit. `None` when it does not
/// exist. It is not the plain sha256 of the content.
async fn content_hash(path: &Path) -> Result<Option<String>> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut hasher = Sha256::new();
    if metadata.is_symlink() {
        let target = tokio::fs::read_link(path).await?;
        hasher.update(format!("symlink:{}", target.display()));
    } else if metadata.is_dir() {
        // A submodule or nested repository: its state is not a file.
        hasher.update(b"directory");
    } else {
        // Read in pieces, so a large file never has to fit in memory.
        let mut file = tokio::fs::File::open(path).await?;
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        if executable(&metadata) {
            hasher.update(b"\0executable");
        }
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

#[cfg(unix)]
fn executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// Refuse to commit unless every file is a file (a directory would commit
/// whatever is under it) in the state recorded by the last `review_changes`,
/// which the agent passes as `reviewed` (`path -> sha256`, null when deleted).
async fn check_reviewed(root: &Path, files: &[String], reviewed: &Value) -> Result<()> {
    let Some(reviewed) = reviewed.as_object() else {
        bail!("review_required: the changes have not been reviewed; call review_changes first");
    };
    let mut unreviewed = Vec::new();
    for file in files {
        let path = root.join(file);
        if tokio::fs::symlink_metadata(&path)
            .await
            .is_ok_and(|metadata| metadata.is_dir())
        {
            bail!("'{file}' is a directory; list the files to commit one by one");
        }
        let current = json!(content_hash(&path).await?);
        if reviewed.get(file) != Some(&current) {
            unreviewed.push(file.as_str());
        }
    }
    if !unreviewed.is_empty() {
        bail!(
            "review_required: these files changed after the last review or were not part of it: {}; call review_changes again before committing them",
            unreviewed.join(", ")
        );
    }
    Ok(())
}

/// A changed path of the workspace, relative to the workspace.
struct Change {
    path: String,
    status: &'static str,
}

/// A git working tree.
struct Git {
    dir: std::path::PathBuf,
    /// An empty directory used as `core.hooksPath`, so no hook runs.
    no_hooks: tempfile::TempDir,
}

impl Git {
    /// The working tree containing `dir`; git commands run in `dir`, so
    /// paths are relative to it.
    async fn open(dir: &Path) -> Result<Self> {
        let git = Self {
            dir: dir.to_path_buf(),
            no_hooks: tempfile::tempdir().context("failed to create a temporary directory")?,
        };
        git.run(&["rev-parse", "--show-toplevel"])
            .await
            .with_context(|| {
                format!("the workspace {} is not in a git repository", dir.display())
            })?;
        Ok(git)
    }

    /// `origin`, or else the first remote, with the URL it pushes to (its
    /// `pushurl` when set) as configured, before `insteadOf` rewrites.
    async fn push_remote(&self) -> Result<Option<(String, String)>> {
        let output = self
            .output(&["config", "--get-regexp", r"^remote\..*\.(url|pushurl)$"])
            .await?;
        let mut remotes: Vec<(String, String)> = Vec::new();
        let mut push_urls: Vec<(String, String)> = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some((key, url)) = line.split_once(' ') else {
                continue;
            };
            let Some(key) = key.strip_prefix("remote.") else {
                continue;
            };
            if let Some(name) = key.strip_suffix(".pushurl") {
                push_urls.push((name.to_string(), url.to_string()));
            } else if let Some(name) = key.strip_suffix(".url") {
                remotes.push((name.to_string(), url.to_string()));
            }
        }
        remotes.sort_by_key(|(name, _)| name != "origin");
        Ok(remotes.into_iter().next().map(|(name, url)| {
            let url = push_urls
                .into_iter()
                .find(|(push, _)| *push == name)
                .map_or(url, |(_, push)| push);
            (name, url)
        }))
    }

    /// The URL `remote` pushes to, after `pushurl` and URL rewrites.
    async fn push_target(&self, remote: &str) -> Result<String> {
        Ok(self
            .run(&["remote", "get-url", "--push", remote])
            .await?
            .trim()
            .to_string())
    }

    /// The default branch of the repository at `url`, asked from it every
    /// time: the `refs/remotes/<remote>/HEAD` a clone recorded goes stale
    /// when the default branch is changed.
    async fn default_branch(&self, url: &str) -> Result<String> {
        let listed = self.run(&["ls-remote", "--symref", url, "HEAD"]).await?;
        listed
            .lines()
            .find_map(|line| {
                let target = line.strip_prefix("ref: refs/heads/")?;
                Some(target.split('\t').next()?.to_string())
            })
            .context("the remote did not report its HEAD branch")
    }

    /// The checked-out branch, or `None` on a detached HEAD.
    async fn current_branch(&self) -> Result<Option<String>> {
        let output = self
            .output(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .await?;
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!branch.is_empty()).then_some(branch))
    }

    async fn ref_exists(&self, reference: &str) -> Result<bool> {
        Ok(self
            .output(&["rev-parse", "--verify", "--quiet", reference])
            .await?
            .status
            .success())
    }

    async fn branch_exists(&self, branch: &str) -> Result<bool> {
        self.ref_exists(&format!("refs/heads/{branch}")).await
    }

    async fn remote_branch_exists(&self, url: &str, branch: &str) -> Result<bool> {
        let reference = format!("refs/heads/{branch}");
        let listed = self
            .run(&["ls-remote", "--heads", url, &reference])
            .await
            .context("could not check the branches of the remote")?;
        Ok(!listed.trim().is_empty())
    }

    /// Whether HEAD has commits `remote` does not have yet: beyond the
    /// remote's copy of `branch`, or beyond its default branch when `branch`
    /// was never pushed.
    async fn unpushed(&self, remote: &str, branch: &str, default_branch: &str) -> Result<bool> {
        let pushed = format!("refs/remotes/{remote}/{branch}");
        let base = if self.ref_exists(&pushed).await? {
            pushed
        } else {
            format!("refs/remotes/{remote}/{default_branch}")
        };
        if !self.ref_exists(&base).await? {
            return Ok(true);
        }
        let count = self
            .run(&["rev-list", "--count", &format!("{base}..HEAD")])
            .await?;
        Ok(count.trim() != "0")
    }

    /// Create `branch` at HEAD and check it out, keeping uncommitted changes.
    async fn create_branch(&self, branch: &str) -> Result<()> {
        self.run(&["check-ref-format", "--branch", branch])
            .await
            .with_context(|| format!("invalid branch name '{branch}'"))?;
        self.run(&["switch", "--quiet", "-c", branch])
            .await
            .map(drop)
    }

    /// The changed files under the workspace (or among `paths`), relative to
    /// the workspace even when it is a subdirectory of the repository.
    async fn changed_paths(&self, paths: Option<&[String]>) -> Result<Vec<Change>> {
        let prefix = self.run(&["rev-parse", "--show-prefix"]).await?;
        let prefix = prefix.trim();
        let mut status = vec![
            "status",
            "--porcelain",
            "-z",
            "--no-renames",
            "--untracked-files=all",
            "--",
        ];
        match paths {
            Some(paths) => status.extend(paths.iter().map(String::as_str)),
            None => status.push("."),
        }
        let output = self.run(&status).await?;
        Ok(output
            .split('\0')
            .filter(|entry| entry.len() > 3)
            .filter_map(|entry| {
                let (code, path) = entry.split_at(3);
                let path = path.strip_prefix(prefix)?.to_string();
                let status = match code.trim() {
                    "??" | "A" | "AM" => "added",
                    code if code.contains('D') => "deleted",
                    _ => "modified",
                };
                Some(Change { path, status })
            })
            .collect())
    }

    /// The diff of `changes` against HEAD, new files included. Returns
    /// whether it was cut at `MAX_DIFF_BYTES`.
    async fn diff(&self, changes: &[Change]) -> Result<(String, bool)> {
        // Without a first commit there is nothing to compare with: every
        // file is new.
        let has_head = self.ref_exists("HEAD").await?;
        let is_new = |change: &&Change| change.status == "added" || !has_head;
        let tracked: Vec<&str> = changes
            .iter()
            .filter(|change| !is_new(change))
            .map(|change| change.path.as_str())
            .collect();
        // Built-in diffs only: external diff and textconv programs are
        // configuration that would run commands.
        let options = [
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
        ];
        // One byte past the limit tells that the text was cut.
        let limit = MAX_DIFF_BYTES + 1;
        let mut text = String::new();
        // Whether a file was left out, which makes the diff incomplete too.
        let mut omitted = false;
        if !tracked.is_empty() {
            let mut args = vec!["diff", "HEAD"];
            args.extend(options);
            args.push("--");
            args.extend(tracked);
            let (output, status) = self.bounded_stdout(&args, limit).await?;
            if status.is_some_and(|status| !status.success()) {
                bail!("git diff failed");
            }
            text.push_str(&output);
        }
        for change in changes.iter().filter(is_new) {
            if text.len() >= limit {
                break;
            }
            let size = tokio::fs::symlink_metadata(self.dir.join(&change.path))
                .await
                .map_or(0, |metadata| metadata.len());
            if size > MAX_DIFF_BYTES as u64 {
                text.push_str(&format!(
                    "new file {} ({size} bytes): diff omitted, too large\n",
                    change.path
                ));
                omitted = true;
                continue;
            }
            let mut args = vec!["diff", "--no-index"];
            args.extend(options);
            args.extend(["--", "/dev/null", change.path.as_str()]);
            // Exits with 1 when the files differ, which they always do.
            let (output, _) = self.bounded_stdout(&args, limit - text.len()).await?;
            text.push_str(&output);
        }
        if text.len() <= MAX_DIFF_BYTES {
            return Ok((text, omitted));
        }
        let mut end = MAX_DIFF_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        Ok((text, true))
    }

    /// The stdout of a git command, read up to `limit` bytes; a command that
    /// writes more is stopped. The exit status is `None` when it was stopped.
    async fn bounded_stdout(
        &self,
        args: &[&str],
        limit: usize,
    ) -> Result<(String, Option<std::process::ExitStatus>)> {
        let mut child = self
            .command(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("failed to run git")?;
        let stdout = child.stdout.take().context("git stdout unavailable")?;
        let mut bytes = Vec::new();
        stdout.take(limit as u64).read_to_end(&mut bytes).await?;
        let status = if bytes.len() < limit {
            Some(child.wait().await?)
        } else {
            None // Dropping the child kills it.
        };
        Ok((String::from_utf8_lossy(&bytes).into_owned(), status))
    }

    /// Whether any of `paths` differs from HEAD, including untracked files.
    async fn has_changes(&self, paths: &[String]) -> Result<bool> {
        let mut status = vec!["status", "--porcelain", "--untracked-files=all", "--"];
        status.extend(paths.iter().map(String::as_str));
        Ok(!self.run(&status).await?.trim().is_empty())
    }

    /// Commit the changes of `paths` and nothing else. Returns the short
    /// hash of the commit.
    async fn commit_paths(&self, message: &str, paths: &[String]) -> Result<String> {
        let mut add = vec!["add", "--all", "--"];
        add.extend(paths.iter().map(String::as_str));
        self.run(&add).await?;
        let mut commit = vec!["commit", "--quiet", "-m", message, "--"];
        commit.extend(paths.iter().map(String::as_str));
        self.run(&commit).await?;
        self.head_commit().await
    }

    async fn head_commit(&self) -> Result<String> {
        Ok(self
            .run(&["rev-parse", "--short", "HEAD"])
            .await?
            .trim()
            .to_string())
    }

    async fn push(&self, remote: &str, branch: &str) -> Result<()> {
        self.run(&["push", "--quiet", "--set-upstream", remote, branch])
            .await
            .map(drop)
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        let output = self.output(args).await?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn output(&self, args: &[&str]) -> Result<std::process::Output> {
        self.command(args)
            .output()
            .await
            .context("failed to run git")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-c")
            .arg(format!("core.hooksPath={}", self.no_hooks.path().display()))
            // fsmonitor only speeds up `git status`; its command would run
            // on every call. Filters stay, since commits need them (LFS).
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(&self.dir)
            // Paths are file names, never pathspec magic such as `:(top)`.
            .env("GIT_LITERAL_PATHSPECS", "1")
            // Fail instead of waiting for a password nobody is asked for.
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn context(workspace: &Path, allow_writes: bool) -> ToolContext {
        ToolContext {
            workspace: Some(workspace.to_path_buf()),
            allow_writes,
            ..ToolContext::default()
        }
    }

    /// Commit with `reviewed` set to the current state of the files, as the
    /// agent passes it after a review of them.
    async fn commit(mut arguments: Value, context: &ToolContext) -> Result<Value> {
        let root = context.workspace.clone().unwrap();
        let mut reviewed = serde_json::Map::new();
        for file in arguments["files"].as_array().into_iter().flatten() {
            if let Some(file) = file.as_str() {
                let hash = content_hash(&root.join(file)).await.unwrap_or_default();
                reviewed.insert(file.to_string(), json!(hash));
            }
        }
        arguments["reviewed"] = Value::Object(reviewed);
        git_commit_push(arguments, context).await
    }

    async fn error(arguments: Value, context: &ToolContext) -> String {
        format!("{:#}", commit(arguments, context).await.unwrap_err())
    }

    /// A clone of `owner/repo` whose GitHub URL is rewritten to a local
    /// bare repository, as `git clone` leaves it.
    async fn clone() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let origin = directory.path().join("origin.git");
        let seed = directory.path().join("seed");
        let work = directory.path().join("work");
        let github = "https://github.com/owner/repo.git";
        let rewrite = format!("url.{}.insteadOf={github}", origin.display());
        std::fs::create_dir_all(&seed).unwrap();
        git(
            directory.path(),
            &[
                "init",
                "--quiet",
                "--bare",
                "--initial-branch=main",
                "origin.git",
            ],
        )
        .await;
        git(&seed, &["init", "--quiet", "--initial-branch=main"]).await;
        std::fs::write(seed.join("README.md"), "hello\n").unwrap();
        std::fs::write(seed.join("old.txt"), "old\n").unwrap();
        git(&seed, &["add", "."]).await;
        git(
            &seed,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "commit",
                "--quiet",
                "-m",
                "init",
            ],
        )
        .await;
        git(
            &seed,
            &["push", "--quiet", origin.to_str().unwrap(), "main"],
        )
        .await;
        git(
            directory.path(),
            &["-c", &rewrite, "clone", "--quiet", github, "work"],
        )
        .await;
        let (key, value) = rewrite.split_once('=').unwrap();
        git(&work, &["config", key, value]).await;
        git(&work, &["config", "user.name", "Test"]).await;
        git(&work, &["config", "user.email", "test@example.com"]).await;
        (directory, origin, work)
    }

    #[tokio::test]
    async fn listed_files_are_committed_and_pushed_on_a_new_branch() {
        let (_directory, origin, work) = clone().await;
        std::fs::write(work.join("README.md"), "changed\n").unwrap();
        std::fs::write(work.join("new.txt"), "new\n").unwrap();
        std::fs::remove_file(work.join("old.txt")).unwrap();
        std::fs::write(work.join("unrelated.txt"), "keep out\n").unwrap();
        let files = json!(["README.md", "new.txt", "old.txt"]);

        let refused = error(
            json!({"files": files, "message": "Fix it"}),
            &context(&work, false),
        )
        .await;
        assert!(refused.contains("allow_writes"), "{refused}");
        let context = context(&work, true);
        for (arguments, expected) in [
            (
                json!({"files": files, "message": "Fix it"}),
                "default branch 'main'",
            ),
            (
                json!({"files": files, "message": "Fix it", "branch": "main"}),
                "default branch of origin",
            ),
            (
                json!({"files": files, "message": "Fix it", "branch": "bad..name"}),
                "invalid branch name",
            ),
            (
                json!({"files": ["../x"], "message": "Fix it", "branch": "b"}),
                "relative path",
            ),
            (
                json!({"files": [".GIT/config"], "message": "Fix it", "branch": "b"}),
                "inside .git",
            ),
            (
                json!({"files": ["README.md"], "message": " ", "branch": "b"}),
                "message",
            ),
            (
                json!({"files": ["missing.txt"], "message": "Fix it", "branch": "b"}),
                "no uncommitted changes",
            ),
        ] {
            let error = error(arguments, &context).await;
            assert!(error.contains(expected), "{error}");
        }
        assert_eq!(
            git(&work, &["branch", "--show-current"]).await.trim(),
            "main"
        );

        let result = commit(
            json!({"files": files, "message": "Fix it (#6)", "branch": "ano/issue-6"}),
            &context,
        )
        .await
        .unwrap();
        assert_eq!(result["branch"], "ano/issue-6");
        assert_eq!(result["created_branch"], true);
        assert_eq!(result["committed"], true);
        assert_eq!(result["default_branch"], "main");
        assert_eq!(result["repository"], "owner/repo");
        assert_eq!(result["host"], "github.com");
        assert_eq!(result["remote"], "origin");

        let pushed = git(
            &origin,
            &["show", "--name-status", "--format=%s", "ano/issue-6"],
        )
        .await;
        assert!(pushed.starts_with("Fix it (#6)"), "{pushed}");
        for line in ["M\tREADME.md", "A\tnew.txt", "D\told.txt"] {
            assert!(pushed.contains(line), "{pushed}");
        }
        assert!(!pushed.contains("unrelated.txt"));
        assert_eq!(
            git(&work, &["status", "--porcelain"]).await,
            "?? unrelated.txt\n"
        );

        // Later fixes go onto the checked-out branch.
        std::fs::write(work.join("README.md"), "changed again\n").unwrap();
        let result = commit(
            json!({"files": ["README.md"], "message": "Address review"}),
            &context,
        )
        .await
        .unwrap();
        assert_eq!(result["created_branch"], false);
        assert_eq!(
            git(&origin, &["log", "-1", "--format=%s", "ano/issue-6"])
                .await
                .trim(),
            "Address review"
        );
        let nothing = error(
            json!({"files": ["README.md"], "message": "Again"}),
            &context,
        )
        .await;
        assert!(nothing.contains("nothing to push"), "{nothing}");
    }

    #[tokio::test]
    async fn diffs_work_before_the_first_commit_and_leave_out_large_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        git(root, &["init", "--quiet", "--initial-branch=main"]).await;
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        std::fs::write(root.join("big.bin"), vec![b'x'; MAX_DIFF_BYTES + 10]).unwrap();

        let result = git_diff(json!({}), &context(root, false)).await.unwrap();
        let files = result["files"].as_array().unwrap();
        assert_eq!(files.len(), 2, "{result}");
        assert!(files.iter().all(|file| file["status"] == "added"));
        let diff = result["diff"].as_str().unwrap();
        assert!(diff.contains("+hello"), "{diff}");
        assert!(
            diff.contains("big.bin") && diff.contains("diff omitted"),
            "{diff}"
        );
        // The left-out file makes the diff incomplete.
        assert_eq!(result["diff_truncated"], true);
        assert!(diff.len() < MAX_DIFF_BYTES);
    }

    #[tokio::test]
    async fn diffs_of_a_subdirectory_workspace_use_its_own_paths() {
        let (_directory, _origin, work) = clone().await;
        std::fs::create_dir_all(work.join("sub/deep")).unwrap();
        std::fs::write(work.join("sub/deep/a.txt"), "new\n").unwrap();
        std::fs::write(work.join("README.md"), "outside the workspace\n").unwrap();

        let result = git_diff(json!({}), &context(&work.join("sub"), false))
            .await
            .unwrap();
        let paths: Vec<&str> = result["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|file| file["path"].as_str())
            .collect();
        assert_eq!(paths, ["deep/a.txt"], "{result}");
        assert!(result["diff"].as_str().unwrap().contains("+new"));
    }

    #[tokio::test]
    async fn only_files_as_they_were_reviewed_are_committed() {
        let (_directory, origin, work) = clone().await;
        let context = context(&work, true);
        std::fs::create_dir_all(work.join("src")).unwrap();
        std::fs::write(work.join("src/a.txt"), "reviewed\n").unwrap();
        let arguments = json!({"files": ["src/a.txt"], "message": "Add", "branch": "add"});
        let review = |hash: Value| {
            let mut arguments = arguments.clone();
            arguments["reviewed"] = json!({ "src/a.txt": hash });
            arguments
        };
        let hash = json!(content_hash(&work.join("src/a.txt")).await.unwrap());

        let missing = format!(
            "{:#}",
            git_commit_push(arguments.clone(), &context)
                .await
                .unwrap_err()
        );
        assert!(missing.contains("not been reviewed"), "{missing}");
        std::fs::write(work.join("src/a.txt"), "changed after the review\n").unwrap();
        let stale = format!(
            "{:#}",
            git_commit_push(review(hash.clone()), &context)
                .await
                .unwrap_err()
        );
        assert!(stale.contains("changed after the last review"), "{stale}");
        // So is a mode change, which git commits too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(work.join("src/a.txt"), "reviewed\n").unwrap();
            let file = work.join("src/a.txt");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mode = format!(
                "{:#}",
                git_commit_push(review(hash.clone()), &context)
                    .await
                    .unwrap_err()
            );
            assert!(mode.contains("changed after the last review"), "{mode}");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        // A directory would commit whatever is under it.
        let mut directory = review(hash.clone());
        directory["files"] = json!(["src"]);
        directory["reviewed"] = json!({"src": content_hash(&work.join("src")).await.unwrap()});
        let refused = format!(
            "{:#}",
            git_commit_push(directory, &context).await.unwrap_err()
        );
        assert!(refused.contains("is a directory"), "{refused}");
        assert_eq!(
            git(&work, &["branch", "--show-current"]).await.trim(),
            "main"
        );

        std::fs::write(work.join("src/a.txt"), "reviewed\n").unwrap();
        // Another spelling of the reviewed path.
        let mut spelled = review(hash);
        spelled["files"] = json!(["./src//a.txt"]);
        git_commit_push(spelled, &context).await.unwrap();
        assert_eq!(git(&origin, &["show", "add:src/a.txt"]).await, "reviewed\n");
    }

    #[tokio::test]
    async fn failed_pushes_can_be_retried_and_hooks_never_run() {
        let (_directory, origin, work) = clone().await;
        let context = context(&work, true);
        // A hook the agent could have written through a tracked hooks path.
        std::fs::create_dir_all(work.join("hooks")).unwrap();
        let hook = work.join("hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git(&work, &["config", "core.hooksPath", "hooks"]).await;

        // A branch that exists only on the remote is refused before any change.
        git(&work, &["push", "--quiet", "origin", "main:taken"]).await;
        std::fs::write(work.join("README.md"), "fixed\n").unwrap();
        let files = json!(["README.md"]);
        // fsmonitor would run on the `git status` of every call.
        let monitor = work.join("hooks/monitor");
        std::fs::write(&monitor, "#!/bin/sh\ntouch monitor-ran\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&monitor, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git(
            &work,
            &["config", "core.fsmonitor", monitor.to_str().unwrap()],
        )
        .await;
        let taken = error(
            json!({"files": files, "message": "Fix", "branch": "taken"}),
            &context,
        )
        .await;
        assert!(taken.contains("already exists on origin"), "{taken}");
        assert!(!work.join("monitor-ran").exists());
        git(&work, &["config", "--unset", "core.fsmonitor"]).await;
        assert_eq!(
            git(&work, &["branch", "--show-current"]).await.trim(),
            "main"
        );

        // The push fails, the commit stays, and a second call pushes it.
        // The remote cannot create `fix` while it has `fix/blocker`, a branch
        // the existence check (for `fix` itself) does not see.
        git(&work, &["push", "--quiet", "origin", "main:fix/blocker"]).await;
        let failed = error(
            json!({"files": files, "message": "Fix", "branch": "fix"}),
            &context,
        )
        .await;
        assert!(failed.contains("call git_commit_push again"), "{failed}");
        assert!(!work.join("hook-ran").exists());
        git(&origin, &["branch", "-D", "fix/blocker"]).await;
        let retried = commit(json!({"files": files, "message": "Fix"}), &context)
            .await
            .unwrap();
        assert_eq!(retried["committed"], false);
        assert_eq!(git(&origin, &["show", "fix:README.md"]).await, "fixed\n");
        assert!(!work.join("hook-ran").exists());
    }

    #[tokio::test]
    async fn the_default_branch_is_asked_from_the_remote() {
        let (_directory, origin, work) = clone().await;
        // The default branch changes on the remote; the clone's origin/HEAD
        // still names main.
        git(&work, &["push", "--quiet", "origin", "main:develop"]).await;
        git(&origin, &["symbolic-ref", "HEAD", "refs/heads/develop"]).await;
        git(&work, &["switch", "--quiet", "-c", "develop"]).await;
        std::fs::write(work.join("README.md"), "changed\n").unwrap();
        let error = error(
            json!({"files": ["README.md"], "message": "Fix"}),
            &context(&work, true),
        )
        .await;
        assert!(error.contains("default branch 'develop'"), "{error}");
        assert_eq!(
            git(&origin, &["log", "-1", "--format=%s", "develop"])
                .await
                .trim(),
            "init"
        );
    }

    #[tokio::test]
    async fn an_unknown_default_branch_stops_the_commit() {
        let (_directory, _origin, work) = clone().await;
        git(&work, &["remote", "set-head", "origin", "--delete"]).await;
        let unreachable = work.join("unreachable.git");
        git(
            &work,
            &["remote", "set-url", "origin", unreachable.to_str().unwrap()],
        )
        .await;
        git(&work, &["switch", "--quiet", "-c", "topic"]).await;
        std::fs::write(work.join("README.md"), "changed\n").unwrap();
        let error = error(
            json!({"files": ["README.md"], "message": "Fix"}),
            &context(&work, true),
        )
        .await;
        assert!(
            error.contains("could not determine the default branch"),
            "{error}"
        );
        assert!(!git(&work, &["status", "--porcelain"]).await.is_empty());
    }
}
