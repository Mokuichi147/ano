//! Names of the built-in tools that the harness's rules refer to.

/// Built-in tool that runs shell commands when `ToolContext::allow_exec` is set.
pub const WORKSPACE_EXEC_NAME: &str = "workspace_exec";

/// Built-in tool that reads a workspace file by offset or by lines.
pub const WORKSPACE_READ_NAME: &str = "workspace_read";

/// Built-in tool that runs a validation check configured for the environment.
pub const WORKSPACE_CHECK_NAME: &str = "workspace_check";

/// Built-in tools that change one workspace file.
pub const WORKSPACE_EDIT_NAME: &str = "workspace_edit";
pub const WORKSPACE_WRITE_NAME: &str = "workspace_write";
/// Built-in tool that reads public web pages; each fetch needs approval.
pub const WEB_FETCH_NAME: &str = "web_fetch";
/// Built-in tool that commits workspace files and pushes them; it changes
/// the checkout, so it needs `ToolContext::allow_writes`.
pub const GIT_COMMIT_PUSH_NAME: &str = "git_commit_push";
/// Built-in tool that shows the uncommitted changes of the workspace, with a
/// content hash per file that reviews are recorded against.
pub const GIT_DIFF_NAME: &str = "git_diff";
/// Runtime tool that has a read-only sub-agent in a fresh conversation
/// review the uncommitted changes. `git_commit_push` only commits files in
/// the state a review last saw.
pub const REVIEW_CHANGES_NAME: &str = "review_changes";

/// Default and maximum `timeout_secs` of one `workspace_exec` command.
pub const EXEC_DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const EXEC_MAX_TIMEOUT_SECS: u64 = 1800;
