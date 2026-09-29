//! Repositories named by git remotes, so the result of `git_commit_push` can
//! be handed to GitHub tools such as those of the GitHub MCP server.

use std::fmt;

/// A repository on a git host, `owner/name` on `host`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    /// The repository a git remote URL points to, in any of the forms git
    /// accepts: `https://host/owner/name(.git)`, `git@host:owner/name(.git)`,
    /// or `ssh://git@host:port/owner/name`. Local paths give `None`.
    pub fn from_remote_url(url: &str) -> Option<Self> {
        let url = url.trim().trim_end_matches('/');
        let (authority, path) = match url.split_once("://") {
            Some(("file", _)) => return None,
            Some((_, rest)) => rest.split_once('/')?,
            // scp-like syntax; a path with a slash before the colon is local.
            None => {
                let (host, path) = url.split_once(':')?;
                if host.contains('/') {
                    return None;
                }
                (host, path)
            }
        };
        let host = authority.rsplit('@').next()?;
        let host = host.split(':').next()?.to_ascii_lowercase();
        let mut parts = path.rsplit('/');
        let name = parts.next()?;
        let name = name.strip_suffix(".git").unwrap_or(name);
        let owner = parts.next()?;
        let valid = |part: &str| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        (!host.is_empty() && valid(owner) && valid(name)).then(|| Self {
            host,
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_name_their_repository_and_host() {
        for (url, host) in [
            (
                "https://github.com/Mokuichi147/issue-test.git",
                "github.com",
            ),
            (
                "https://token@github.com/Mokuichi147/issue-test",
                "github.com",
            ),
            ("git@github.com:Mokuichi147/issue-test.git", "github.com"),
            ("ssh://git@github.com/Mokuichi147/issue-test", "github.com"),
            (
                "ssh://git@GHE.example.com:2222/Mokuichi147/issue-test",
                "ghe.example.com",
            ),
            (
                "https://github.example.com/Mokuichi147/issue-test/",
                "github.example.com",
            ),
        ] {
            let repo = RepoRef::from_remote_url(url).expect(url);
            assert_eq!(repo.to_string(), "Mokuichi147/issue-test", "{url}");
            assert_eq!(repo.host, host, "{url}");
        }
        for url in [
            "/tmp/owner/issue-test",
            "./owner/issue-test",
            "file:///tmp/owner/issue-test",
            "issue-test",
            "https://github.com/issue-test",
            "https://github.com/owner/.hidden",
        ] {
            assert_eq!(RepoRef::from_remote_url(url), None, "{url}");
        }
    }
}
