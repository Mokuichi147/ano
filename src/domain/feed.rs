//! The feed: what the agent leaves for the user to look at later, such as
//! ideas drawn from what it knows about the user, articles that match their
//! interests, and short reports of work it did on its own. The user rates
//! items and marks them done, and later runs read that to do better.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Tool that adds an item to the user's feed.
pub const FEED_POST_NAME: &str = "feed_post";
/// Tool that reads recent items with the user's ratings.
pub const FEED_READ_NAME: &str = "feed_read";

pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_BODY_CHARS: usize = 2000;
pub const MAX_ACTION_CHARS: usize = 500;
pub const MAX_URL_CHARS: usize = 2000;
/// Items kept per user; the oldest go first.
pub const MAX_ITEMS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedKind {
    /// Something the user might want to do, with what to ask the agent.
    Idea,
    /// A page that matches the user's interests.
    Article,
    /// What the agent did on its own, such as a scheduled task.
    Report,
}

impl FeedKind {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "idea" => Self::Idea,
            "article" => Self::Article,
            "report" => Self::Report,
            _ => bail!("kind must be idea, article, or report: {text:?}"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Idea => "idea",
            Self::Article => "article",
            Self::Report => "report",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedItem {
    /// Short and stable, such as `f12`.
    pub id: String,
    pub kind: FeedKind,
    pub title: String,
    /// Markdown.
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// For ideas: what to ask the agent to carry the idea out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    pub created_at: u64,
    /// The user's rating: 1 liked, -1 not wanted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<i8>,
    /// The user read it, or carried the idea out.
    #[serde(default)]
    pub done: bool,
}

/// The fields of a new item, checked. `url` must be http(s).
pub fn check_item(
    title: &str,
    body: &str,
    url: Option<&str>,
    action: Option<&str>,
) -> Result<(String, String, Option<String>, Option<String>)> {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        bail!("title must be 1 to {MAX_TITLE_CHARS} characters");
    }
    let body = body.trim().to_string();
    if body.chars().count() > MAX_BODY_CHARS {
        bail!("body must be at most {MAX_BODY_CHARS} characters");
    }
    let url = url.map(str::trim).filter(|url| !url.is_empty());
    if let Some(url) = url {
        if !(url.starts_with("https://") || url.starts_with("http://"))
            || url.chars().count() > MAX_URL_CHARS
            || url.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            bail!("url must be one http(s) URL");
        }
    }
    let action = action.map(str::trim).filter(|action| !action.is_empty());
    if action.is_some_and(|action| action.chars().count() > MAX_ACTION_CHARS) {
        bail!("action must be at most {MAX_ACTION_CHARS} characters");
    }
    Ok((
        title,
        body,
        url.map(str::to_string),
        action.map(str::to_string),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_need_a_title_and_bounded_fields() {
        let (title, body, url, action) = check_item(
            " Try a \n morning walk ",
            " You said you sleep badly.\n",
            Some(" https://example.com/walk "),
            Some(" Plan a 20-minute walk route near home "),
        )
        .unwrap();
        assert_eq!(title, "Try a morning walk");
        assert_eq!(body, "You said you sleep badly.");
        assert_eq!(url.as_deref(), Some("https://example.com/walk"));
        assert_eq!(
            action.as_deref(),
            Some("Plan a 20-minute walk route near home")
        );
        assert_eq!(check_item("t", "", Some(" "), None).unwrap().2, None);
        for (title, url) in [
            (" ", None),
            ("t", Some("javascript:alert(1)")),
            ("t", Some("https://a b")),
        ] {
            assert!(check_item(title, "", url, None).is_err(), "{title} {url:?}");
        }
        assert!(check_item("t", &"b".repeat(MAX_BODY_CHARS + 1), None, None).is_err());
        assert_eq!(FeedKind::parse("idea").unwrap(), FeedKind::Idea);
        assert!(FeedKind::parse("news").is_err());
    }
}
