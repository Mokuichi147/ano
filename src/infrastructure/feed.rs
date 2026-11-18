//! The feed kept as a `feed.json` file, and the `feed_post` and `feed_read`
//! tools.
//!
//! Each user has one file under the platform's data directory (for example
//! `~/Library/Application Support/ano/feed/<user>/feed.json` on macOS).

use crate::{
    application::{clock::unix_now, registry::ToolRegistry},
    domain::{
        feed::{check_item, FeedItem, FeedKind, FEED_POST_NAME, FEED_READ_NAME, MAX_ITEMS},
        tool::ToolDefinition,
    },
    infrastructure::fs::{atomic_write, lock_in, user_directory},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs::File, io::ErrorKind, path::PathBuf, sync::Arc};

const FEED_FILE: &str = "feed.json";
const LOCK_FILE: &str = "feed.json.lock";
/// Items `feed_read` returns at most.
const MAX_READ_ITEMS: usize = 50;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FeedSettings {
    pub enabled: bool,
    /// Where the feed is kept. Defaults to `feed` in the platform's data
    /// directory for ano.
    pub dir: Option<PathBuf>,
}

impl FeedSettings {
    pub fn directory(&self) -> Result<PathBuf> {
        match &self.dir {
            Some(dir) => Ok(dir.clone()),
            None => default_feed_dir()
                .context("cannot determine the data directory for the feed; set feed.dir"),
        }
    }
}

/// `feed` in the platform's data directory: `$XDG_DATA_HOME/ano` (or
/// `~/.local/share/ano`) on Linux, `~/Library/Application Support/ano` on
/// macOS, and `%APPDATA%\ano\data` on Windows.
pub fn default_feed_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ano").map(|dirs| dirs.data_dir().join("feed"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FeedFile {
    #[serde(default)]
    last_id: u64,
    items: Vec<FeedItem>,
}

pub struct Feed {
    root: PathBuf,
}

impl Feed {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The feed when it is enabled, with its tools registered.
    pub fn from_settings(
        settings: &FeedSettings,
        registry: &ToolRegistry,
    ) -> Result<Option<Arc<Self>>> {
        if !settings.enabled {
            return Ok(None);
        }
        let feed = Arc::new(Self::new(settings.directory()?));
        feed.register_tools(registry)?;
        Ok(Some(feed))
    }

    pub fn path(&self, user: &str) -> Result<PathBuf> {
        Ok(user_directory(&self.root, user)?.join(FEED_FILE))
    }

    /// The items of `user`, newest first.
    pub fn items(&self, user: &str) -> Result<Vec<FeedItem>> {
        let mut items = self.load(user)?.items;
        items.reverse();
        Ok(items)
    }

    /// Add an item for `user`; the oldest beyond the limit are dropped.
    pub fn post(
        &self,
        user: &str,
        kind: FeedKind,
        title: &str,
        body: &str,
        url: Option<&str>,
        action: Option<&str>,
    ) -> Result<FeedItem> {
        let (title, body, url, action) = check_item(title, body, url, action)?;
        let _lock = self.lock(user)?;
        let mut file = self.load(user)?;
        file.last_id += 1;
        let item = FeedItem {
            id: format!("f{}", file.last_id),
            kind,
            title,
            body,
            url,
            action,
            created_at: unix_now(),
            rating: None,
            done: false,
        };
        file.items.push(item.clone());
        if file.items.len() > MAX_ITEMS {
            let excess = file.items.len() - MAX_ITEMS;
            file.items.drain(..excess);
        }
        self.store(user, &file)?;
        Ok(item)
    }

    /// Rate the item `id`: 1, -1, or 0 to clear the rating.
    pub fn rate(&self, user: &str, id: &str, rating: i8) -> Result<FeedItem> {
        if !matches!(rating, -1..=1) {
            bail!("rating must be 1, -1, or 0");
        }
        self.modify(user, id, |item| {
            item.rating = (rating != 0).then_some(rating)
        })
    }

    /// Mark the item `id` done, or not.
    pub fn mark_done(&self, user: &str, id: &str, done: bool) -> Result<FeedItem> {
        self.modify(user, id, |item| item.done = done)
    }

    fn modify(&self, user: &str, id: &str, change: impl FnOnce(&mut FeedItem)) -> Result<FeedItem> {
        let _lock = self.lock(user)?;
        let mut file = self.load(user)?;
        let item = file
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .with_context(|| format!("no feed item {id:?}"))?;
        change(item);
        let changed = item.clone();
        self.store(user, &file)?;
        Ok(changed)
    }

    fn load(&self, user: &str) -> Result<FeedFile> {
        let path = self.path(user)?;
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("invalid feed file {}", path.display())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(FeedFile::default()),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn store(&self, user: &str, file: &FeedFile) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(file)?;
        bytes.push(b'\n');
        atomic_write(&self.path(user)?, &bytes, None)
    }

    fn lock(&self, user: &str) -> Result<File> {
        let path = self.path(user)?;
        let directory = path.parent().context("feed file has no directory")?;
        lock_in(directory, LOCK_FILE).with_context(|| {
            format!(
                "the feed is being changed by another process: {}",
                path.display()
            )
        })
    }

    pub fn register_tools(self: &Arc<Self>, registry: &ToolRegistry) -> Result<()> {
        if !registry.is_registered(FEED_POST_NAME) {
            let feed = Arc::clone(self);
            // url and action are optional, which a strict Responses schema
            // cannot express; the handler checks the arguments.
            let mut definition = ToolDefinition::new(
                FEED_POST_NAME,
                "Leave an item in the user's feed, which they read later in ano web's home: an idea for them (with the request that would carry it out), an article that matches their interests, or a short report of work done on your own.",
                json!({
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "enum": ["idea", "article", "report"]},
                        "title": {"type": "string", "description": "A short headline in the user's language."},
                        "body": {"type": "string", "description": "Markdown, at most 2000 characters: why it is for the user, or the summary."},
                        "url": {"type": "string", "description": "The page of an article."},
                        "action": {"type": "string", "description": "For ideas: what the user would ask you to carry it out, in their words."}
                    },
                    "required": ["kind", "title", "body"],
                    "additionalProperties": false
                }),
            )
            .always_offered();
            definition.strict = false;
            registry.register_contextual(definition, move |arguments, context| {
                let feed = Arc::clone(&feed);
                async move {
                    tokio::task::spawn_blocking(move || {
                        feed.post_tool(&context.user_id, &arguments)
                    })
                    .await
                    .context("feed_post task failed")?
                }
            })?;
        }
        if !registry.is_registered(FEED_READ_NAME) {
            let feed = Arc::clone(self);
            registry.register_contextual(
                ToolDefinition::new(
                    FEED_READ_NAME,
                    "Read the user's recent feed items, newest first, with their ratings (1 liked, -1 not wanted) and whether they are done, to avoid repeats and match what the user likes.",
                    json!({
                        "type": "object",
                        "properties": {"limit": {"type": "integer", "description": "How many items, at most 50."}},
                        "required": ["limit"],
                        "additionalProperties": false
                    }),
                )
                .always_offered(),
                move |arguments, context| {
                    let feed = Arc::clone(&feed);
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let limit = arguments["limit"]
                                .as_u64()
                                .unwrap_or(20)
                                .clamp(1, MAX_READ_ITEMS as u64) as usize;
                            let items: Vec<FeedItem> =
                                feed.items(&context.user_id)?.into_iter().take(limit).collect();
                            Ok(json!({"items": items}))
                        })
                        .await
                        .context("feed_read task failed")?
                    }
                },
            )?;
        }
        Ok(())
    }

    fn post_tool(&self, user: &str, arguments: &Value) -> Result<Value> {
        let text = |key: &str| -> Result<Option<&str>> {
            match &arguments[key] {
                Value::Null => Ok(None),
                Value::String(text) => Ok(Some(text.as_str())),
                _ => bail!("{key} must be a string"),
            }
        };
        let kind = FeedKind::parse(text("kind")?.context("kind is required")?)?;
        let item = self.post(
            user,
            kind,
            text("title")?.context("title is required")?,
            text("body")?.unwrap_or_default(),
            text("url")?,
            text("action")?,
        )?;
        Ok(json!({"status": "posted", "id": item.id}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_posted_rated_and_read_newest_first() {
        let root = tempfile::tempdir().unwrap();
        let feed = Feed::new(root.path());
        assert!(feed.items("default").unwrap().is_empty());
        feed.post_tool(
            "default",
            &json!({"kind": "idea", "title": "Morning walk", "body": "You sleep badly.", "action": "Plan a walk"}),
        )
        .unwrap();
        feed.post_tool(
            "default",
            &json!({"kind": "article", "title": "Tea", "body": "", "url": "https://example.com/tea"}),
        )
        .unwrap();
        assert!(feed
            .post_tool(
                "default",
                &json!({"kind": "news", "title": "x", "body": ""})
            )
            .is_err());
        let items = feed.items("default").unwrap();
        assert_eq!(items[0].id, "f2");
        assert_eq!(items[1].action.as_deref(), Some("Plan a walk"));

        assert_eq!(feed.rate("default", "f1", 1).unwrap().rating, Some(1));
        assert_eq!(feed.rate("default", "f1", 0).unwrap().rating, None);
        assert!(feed.rate("default", "f1", 3).is_err());
        assert!(feed.mark_done("default", "f2", true).unwrap().done);
        assert!(feed.rate("default", "f9", 1).is_err());
    }

    #[test]
    fn only_the_newest_items_are_kept() {
        let root = tempfile::tempdir().unwrap();
        let feed = Feed::new(root.path());
        for n in 0..MAX_ITEMS + 3 {
            feed.post(
                "default",
                FeedKind::Report,
                &format!("r{n}"),
                "",
                None,
                None,
            )
            .unwrap();
        }
        let items = feed.items("default").unwrap();
        assert_eq!(items.len(), MAX_ITEMS);
        assert_eq!(items[0].title, format!("r{}", MAX_ITEMS + 2));
        assert_eq!(items.last().unwrap().title, "r3");
    }
}
