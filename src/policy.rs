use serde::Deserialize;

/// Per-user tool restrictions.
///
/// Rules are exact names by default and support a trailing `*` wildcard. Local
/// function names are matched directly. MCP tools are addressed as either
/// `server_label:tool_name` or `mcp:server_label:tool_name`.
///
/// Deny rules are fail-safe: a bare name such as `delete_*` also blocks MCP
/// tools with a matching name on every server. Allow rules are strict: an MCP
/// tool is allowed only by a server-qualified rule (or a global `*`), so a
/// local allow rule never unlocks a same-named MCP tool.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserPolicy {
    pub disabled_tools: Vec<String>,
    /// Optional allowlist. When present, only matching local or MCP tool names
    /// are exposed to the model and accepted at execution time.
    pub allowed_tools: Option<Vec<String>>,
    /// Additional allowlists layered on by an execution environment. A tool
    /// must be allowed by every layer.
    #[serde(skip)]
    extra_allowlists: Vec<Vec<String>>,
}

impl UserPolicy {
    pub fn new(disabled_tools: Vec<String>, allowed_tools: Option<Vec<String>>) -> Self {
        Self {
            disabled_tools,
            allowed_tools,
            extra_allowlists: Vec::new(),
        }
    }

    pub fn with_extra_disabled<I, S>(&self, extra: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut policy = self.clone();
        policy
            .disabled_tools
            .extend(extra.into_iter().map(Into::into));
        policy
    }

    /// Narrow this policy with an environment's allowlist and denylist. The
    /// result never allows anything that either the user or the environment
    /// forbids.
    pub fn with_restrictions(
        &self,
        allowed_tools: Option<&[String]>,
        extra_disabled: &[String],
    ) -> Self {
        let mut policy = self.with_extra_disabled(extra_disabled.iter().cloned());
        if let Some(rules) = allowed_tools {
            policy.extra_allowlists.push(rules.to_vec());
        }
        policy
    }

    pub fn has_allowlist(&self) -> bool {
        self.allowed_tools.is_some() || !self.extra_allowlists.is_empty()
    }

    pub fn is_disabled(&self, tool_name: &str) -> bool {
        self.disabled_tools
            .iter()
            .any(|rule| rule_matches(rule, tool_name))
    }

    pub fn is_allowed(&self, tool_name: &str) -> bool {
        self.allowed_by_every_layer(&[tool_name], |_| true)
    }

    pub fn is_mcp_tool_disabled(&self, server_label: &str, tool_name: &str) -> bool {
        let qualified = format!("{server_label}:{tool_name}");
        let prefixed = format!("mcp:{server_label}:{tool_name}");

        self.is_mcp_server_disabled(server_label)
            || self.is_disabled(tool_name)
            || self.is_disabled(&qualified)
            || self.is_disabled(&prefixed)
    }

    pub fn is_mcp_tool_allowed(&self, server_label: &str, tool_name: &str) -> bool {
        let qualified = format!("{server_label}:{tool_name}");
        let prefixed = format!("mcp:{server_label}:{tool_name}");

        self.allowed_by_every_layer(&[&qualified, &prefixed], |rule| {
            rule == "*" || rule.contains(':')
        })
    }

    pub fn is_mcp_server_disabled(&self, server_label: &str) -> bool {
        let server_wildcard = format!("{server_label}:*");
        let prefixed_server = format!("mcp:{server_label}");
        let prefixed_wildcard = format!("mcp:{server_label}:*");

        self.is_disabled(&server_wildcard)
            || self.is_disabled(&prefixed_server)
            || self.is_disabled(&prefixed_wildcard)
    }

    fn allowed_by_every_layer(&self, names: &[&str], valid_rule: impl Fn(&str) -> bool) -> bool {
        self.allowed_tools
            .iter()
            .chain(self.extra_allowlists.iter())
            .all(|rules| {
                rules.iter().any(|rule| {
                    valid_rule(rule) && names.iter().any(|name| rule_matches(rule, name))
                })
            })
    }
}

fn rule_matches(rule: &str, target: &str) -> bool {
    match rule.strip_suffix('*') {
        Some(prefix) => target.starts_with(prefix),
        None => rule == target,
    }
}

#[cfg(test)]
mod tests {
    use super::UserPolicy;

    #[test]
    fn matches_local_exact_and_wildcard_rules() {
        let policy = UserPolicy::new(vec!["shell_exec".into(), "fs_*".into()], None);

        assert!(policy.is_disabled("shell_exec"));
        assert!(!policy.is_disabled("shell_read"));
        assert!(policy.is_disabled("fs_read"));
        assert!(!policy.is_disabled("network_read"));
    }

    #[test]
    fn matches_qualified_mcp_rules() {
        let policy = UserPolicy::new(vec!["mcp:github:delete_issue".into()], None);

        assert!(policy.is_mcp_tool_disabled("github", "delete_issue"));
        assert!(!policy.is_mcp_tool_disabled("github", "list_issues"));
    }

    #[test]
    fn server_wildcard_disables_every_tool_on_the_server() {
        let policy = UserPolicy::new(vec!["github:*".into()], None);

        assert!(policy.is_mcp_server_disabled("github"));
        assert!(policy.is_mcp_tool_disabled("github", "list_issues"));
        assert!(!policy.is_mcp_tool_disabled("docs", "list_issues"));
    }

    #[test]
    fn local_allow_rules_do_not_unlock_mcp_tools() {
        let policy = UserPolicy::new(vec![], Some(vec!["read_*".into(), "echo".into()]));

        assert!(policy.is_allowed("read_file"));
        assert!(!policy.is_mcp_tool_allowed("files", "read_file"));
        assert!(!policy.is_mcp_tool_allowed("echo", "anything"));

        let qualified = UserPolicy::new(vec![], Some(vec!["mcp:files:read_*".into()]));
        assert!(qualified.is_mcp_tool_allowed("files", "read_file"));
        assert!(!qualified.is_mcp_tool_allowed("other", "read_file"));
    }

    #[test]
    fn bare_deny_rules_also_block_mcp_tools() {
        let policy = UserPolicy::new(vec!["delete_*".into()], None);

        assert!(policy.is_mcp_tool_disabled("github", "delete_issue"));
    }

    #[test]
    fn local_wildcard_allow_rules_do_not_unlock_similarly_named_mcp_servers() {
        let policy = UserPolicy::new(vec![], Some(vec!["read_*".into(), "mcp*".into()]));

        assert!(policy.is_allowed("read_file"));
        assert!(!policy.is_mcp_tool_allowed("read_files", "delete_all"));
        assert!(!policy.is_mcp_tool_allowed("files", "delete_all"));

        let restricted = UserPolicy::new(vec![], Some(vec!["*".into()]))
            .with_restrictions(Some(&["read_*".into()]), &[]);
        assert!(!restricted.is_mcp_tool_allowed("read_files", "delete_all"));

        let qualified = UserPolicy::new(vec![], Some(vec!["read_files:*".into()]));
        assert!(qualified.is_mcp_tool_allowed("read_files", "read_file"));
        let all_mcp = UserPolicy::new(vec![], Some(vec!["mcp:*".into()]));
        assert!(all_mcp.is_mcp_tool_allowed("files", "read_file"));
    }

    #[test]
    fn environment_allowlist_narrows_user_allowlist() {
        let policy = UserPolicy::new(vec![], Some(vec!["echo".into(), "read_file".into()]));
        let restricted =
            policy.with_restrictions(Some(&["echo".to_string(), "write_file".to_string()]), &[]);

        assert!(restricted.is_allowed("echo"));
        assert!(!restricted.is_allowed("read_file"));
        assert!(!restricted.is_allowed("write_file"));
    }

    #[test]
    fn layered_allowlists_accept_different_mcp_spellings() {
        let policy = UserPolicy::new(vec![], Some(vec!["mcp:github:*".into()]));
        let restricted = policy.with_restrictions(Some(&["github:list_*".to_string()]), &[]);

        assert!(restricted.is_mcp_tool_allowed("github", "list_issues"));
        assert!(!restricted.is_mcp_tool_allowed("github", "delete_issue"));
    }

    #[test]
    fn wildcard_allowlists_intersect() {
        let policy = UserPolicy::new(vec![], Some(vec!["workspace_*".into()]));
        let restricted = policy.with_restrictions(
            Some(&["workspace_read".to_string(), "workspace_write".to_string()]),
            &[],
        );

        assert!(restricted.is_allowed("workspace_read"));
        assert!(restricted.is_allowed("workspace_write"));
        assert!(!restricted.is_allowed("workspace_list"));
    }
}
