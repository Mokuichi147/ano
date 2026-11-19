//! Spaces kept as folders, and the `space_save`, `space_read`, and
//! `space_data_set` tools.
//!
//! Each user has a folder of spaces under the platform's data directory
//! (for example `~/Library/Application Support/ano/spaces/<user>/<name>/` on
//! macOS), each with `index.html`, `data.json`, and `space.json` (its title
//! and times).

use crate::{
    application::{clock::unix_now, registry::ToolRegistry},
    domain::{
        space::{
            check_html, check_title, validate_space_name, MAX_DATA_BYTES, MAX_SPACES,
            SPACE_DATA_NAME, SPACE_READ_NAME, SPACE_SAVE_NAME,
        },
        tool::ToolDefinition,
    },
    infrastructure::fs::{atomic_write, lock_in, user_directory},
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs::File, io::ErrorKind, path::PathBuf, sync::Arc};

const PAGE_FILE: &str = "index.html";
const DATA_FILE: &str = "data.json";
const META_FILE: &str = "space.json";
const LOCK_FILE: &str = "spaces.lock";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpaceSettings {
    pub enabled: bool,
    /// Where spaces are kept. Defaults to `spaces` in the platform's data
    /// directory for ano.
    pub dir: Option<PathBuf>,
}

impl SpaceSettings {
    pub fn directory(&self) -> Result<PathBuf> {
        match &self.dir {
            Some(dir) => Ok(dir.clone()),
            None => default_spaces_dir()
                .context("cannot determine the data directory for spaces; set spaces.dir"),
        }
    }
}

/// `spaces` in the platform's data directory.
pub fn default_spaces_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ano").map(|dirs| dirs.data_dir().join("spaces"))
}

/// What is kept about a space besides its page and data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceInfo {
    pub name: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
}

pub struct Spaces {
    root: PathBuf,
}

impl Spaces {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The spaces when they are enabled, with their tools registered.
    pub fn from_settings(
        settings: &SpaceSettings,
        registry: &ToolRegistry,
    ) -> Result<Option<Arc<Self>>> {
        if !settings.enabled {
            return Ok(None);
        }
        let spaces = Arc::new(Self::new(settings.directory()?));
        spaces.register_tools(registry)?;
        Ok(Some(spaces))
    }

    fn user_dir(&self, user: &str) -> Result<PathBuf> {
        user_directory(&self.root, user)
    }

    fn folder(&self, user: &str, name: &str) -> Result<PathBuf> {
        validate_space_name(name)?;
        Ok(self.user_dir(user)?.join(name))
    }

    /// The spaces of `user`, most recently updated first.
    pub fn list(&self, user: &str) -> Result<Vec<SpaceInfo>> {
        let directory = self.user_dir(user)?;
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", directory.display()))
            }
        };
        let mut spaces: Vec<SpaceInfo> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| {
                let bytes = std::fs::read(entry.path().join(META_FILE)).ok()?;
                let info: SpaceInfo = serde_json::from_slice(&bytes).ok()?;
                (entry.file_name().to_str() == Some(info.name.as_str())).then_some(info)
            })
            .collect();
        spaces.sort_by_key(|space| std::cmp::Reverse(space.updated_at));
        Ok(spaces)
    }

    /// The space `name` of `user`: its title, page, and data.
    pub fn read(&self, user: &str, name: &str) -> Result<Option<(SpaceInfo, String, Value)>> {
        let folder = self.folder(user, name)?;
        let info = match std::fs::read(folder.join(META_FILE)) {
            Ok(bytes) => serde_json::from_slice::<SpaceInfo>(&bytes)
                .with_context(|| format!("invalid space {name}"))?,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("failed to read the space"),
        };
        let html = std::fs::read_to_string(folder.join(PAGE_FILE))
            .with_context(|| format!("failed to read the page of space {name}"))?;
        let data = match std::fs::read(folder.join(DATA_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            Err(_) => Value::Null,
        };
        Ok(Some((info, html, data)))
    }

    /// Create the space `name`, or replace its page and title. Its data
    /// stays unless `data` is given.
    pub fn save(
        &self,
        user: &str,
        name: &str,
        title: &str,
        html: &str,
        data: Option<&Value>,
    ) -> Result<(SpaceInfo, bool)> {
        let title = check_title(title)?;
        check_html(html)?;
        if let Some(data) = data {
            check_data(data)?;
        }
        let _lock = self.lock(user)?;
        let folder = self.folder(user, name)?;
        let existing = self.read(user, name)?;
        if existing.is_none() && self.list(user)?.len() >= MAX_SPACES {
            bail!("{MAX_SPACES} spaces are kept already; remove one first");
        }
        let now = unix_now();
        let info = SpaceInfo {
            name: name.to_string(),
            title,
            created_at: existing
                .as_ref()
                .map_or(now, |(info, _, _)| info.created_at),
            updated_at: now,
        };
        std::fs::create_dir_all(&folder)
            .with_context(|| format!("failed to create {}", folder.display()))?;
        atomic_write(&folder.join(PAGE_FILE), html.as_bytes(), None)?;
        if let Some(data) = data {
            atomic_write(&folder.join(DATA_FILE), &serde_json::to_vec(data)?, None)?;
        }
        atomic_write(
            &folder.join(META_FILE),
            &serde_json::to_vec_pretty(&info)?,
            None,
        )?;
        Ok((info, existing.is_none()))
    }

    /// Replace the data of the space `name`.
    pub fn set_data(&self, user: &str, name: &str, data: &Value) -> Result<()> {
        check_data(data)?;
        let _lock = self.lock(user)?;
        let Some((mut info, _, _)) = self.read(user, name)? else {
            bail!("no space {name:?}");
        };
        let folder = self.folder(user, name)?;
        atomic_write(&folder.join(DATA_FILE), &serde_json::to_vec(data)?, None)?;
        info.updated_at = unix_now();
        atomic_write(
            &folder.join(META_FILE),
            &serde_json::to_vec_pretty(&info)?,
            None,
        )
    }

    pub fn remove(&self, user: &str, name: &str) -> Result<()> {
        let _lock = self.lock(user)?;
        let folder = self.folder(user, name)?;
        if !folder.join(META_FILE).exists() {
            bail!("no space {name:?}");
        }
        std::fs::remove_dir_all(&folder)
            .with_context(|| format!("failed to remove {}", folder.display()))
    }

    fn lock(&self, user: &str) -> Result<File> {
        lock_in(&self.user_dir(user)?, LOCK_FILE)
            .context("spaces are being changed by another process")
    }

    pub fn register_tools(self: &Arc<Self>, registry: &ToolRegistry) -> Result<()> {
        if !registry.is_registered(SPACE_SAVE_NAME) {
            let spaces = Arc::clone(self);
            let mut definition = ToolDefinition::new(
                SPACE_SAVE_NAME,
                "Create a space, a small page for the user such as a tracker, planner, or board, or replace one's page. It is shown in ano web, isolated: its scripts cannot connect anywhere and read only their data from window.ANO_SPACE.data.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Lowercase letters, digits, and hyphens, such as weekly-planner. Reuse a name to replace that space."},
                        "title": {"type": "string", "description": "A short title in the user's language."},
                        "html": {"type": "string", "description": "A complete HTML page with inline CSS and JavaScript, at most 200 KiB, that renders window.ANO_SPACE.data and fits a phone screen. Nothing can be loaded from elsewhere."},
                        "data": {"description": "The JSON data the page shows, at most 64 KiB. Omit to keep the current data."}
                    },
                    "required": ["name", "title", "html"],
                    "additionalProperties": false
                }),
            )
            .always_offered();
            definition.strict = false;
            registry.register_contextual(definition, move |arguments, context| {
                let spaces = Arc::clone(&spaces);
                async move {
                    tokio::task::spawn_blocking(move || {
                        let text = |key: &str| {
                            arguments[key].as_str().with_context(|| {
                                format!("{SPACE_SAVE_NAME}.{key} must be a string")
                            })
                        };
                        let data = arguments.get("data").filter(|data| !data.is_null());
                        let (info, created) = spaces.save(
                            &context.user_id,
                            text("name")?,
                            text("title")?,
                            text("html")?,
                            data,
                        )?;
                        Ok(json!({
                            "status": if created { "created" } else { "updated" },
                            "path": format!("/spaces/{}", info.name),
                        }))
                    })
                    .await
                    .context("space_save task failed")?
                }
            })?;
        }
        if !registry.is_registered(SPACE_READ_NAME) {
            let spaces = Arc::clone(self);
            registry.register_contextual(
                ToolDefinition::new(
                    SPACE_READ_NAME,
                    "Read a space's page and data to change them, or list the spaces when name is an empty string.",
                    json!({
                        "type": "object",
                        "properties": {"name": {"type": "string"}},
                        "required": ["name"],
                        "additionalProperties": false
                    }),
                ),
                move |arguments, context| {
                    let spaces = Arc::clone(&spaces);
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let name = arguments["name"].as_str().unwrap_or_default().trim();
                            if name.is_empty() {
                                return Ok(json!({"spaces": spaces.list(&context.user_id)?}));
                            }
                            match spaces.read(&context.user_id, name)? {
                                Some((info, html, data)) => {
                                    Ok(json!({"space": info, "html": html, "data": data}))
                                }
                                None => bail!("no space {name:?}"),
                            }
                        })
                        .await
                        .context("space_read task failed")?
                    }
                },
            )?;
        }
        if !registry.is_registered(SPACE_DATA_NAME) {
            let spaces = Arc::clone(self);
            let mut definition = ToolDefinition::new(
                SPACE_DATA_NAME,
                "Replace the data a space shows, for example with fresh numbers from a scheduled task. The page stays as it is.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "data": {"description": "The whole new JSON data, at most 64 KiB."}
                    },
                    "required": ["name", "data"],
                    "additionalProperties": false
                }),
            );
            definition.strict = false;
            registry.register_contextual(definition, move |arguments, context| {
                let spaces = Arc::clone(&spaces);
                async move {
                    tokio::task::spawn_blocking(move || {
                        let name = arguments["name"]
                            .as_str()
                            .context("space_data_set.name must be a string")?;
                        spaces.set_data(&context.user_id, name, &arguments["data"])?;
                        Ok(json!({"status": "updated"}))
                    })
                    .await
                    .context("space_data_set task failed")?
                }
            })?;
        }
        Ok(())
    }
}

fn check_data(data: &Value) -> Result<()> {
    if serde_json::to_vec(data)?.len() > MAX_DATA_BYTES {
        bail!("data must be at most {} KiB", MAX_DATA_BYTES / 1024);
    }
    Ok(())
}

/// The page of a space as served: its HTML with the data given to its
/// scripts as `window.ANO_SPACE`, placed so it runs before the page's own
/// scripts.
pub fn render_page(info: &SpaceInfo, html: &str, data: &Value) -> String {
    let given = json!({"name": info.name, "title": info.title, "data": data});
    // Inside a script element, only "</" (and "<!--") can end it early.
    let given = given
        .to_string()
        .replace("</", "<\\/")
        .replace("<!--", "<\\!--");
    let script = format!("<script>window.ANO_SPACE = {given};</script>");
    // Right after the head's start tag, which may have attributes; without
    // one, at the start. ASCII lowercasing keeps byte offsets.
    let lower = html.to_ascii_lowercase();
    let head_end = lower
        .match_indices("<head")
        .find(|(index, _)| {
            matches!(
                lower.as_bytes().get(index + "<head".len()),
                Some(b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/')
            )
        })
        .and_then(|(index, _)| lower[index..].find('>').map(|end| index + end + 1));
    match head_end {
        Some(at) => format!("{}{script}{}", &html[..at], &html[at..]),
        None => format!("{script}{html}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaces_are_saved_read_updated_and_removed() {
        let root = tempfile::tempdir().unwrap();
        let spaces = Spaces::new(root.path());
        assert!(spaces.list("default").unwrap().is_empty());
        let (info, created) = spaces
            .save(
                "default",
                "habits",
                "習慣",
                "<p>hi</p>",
                Some(&json!({"days": 3})),
            )
            .unwrap();
        assert!(created);
        assert_eq!(info.title, "習慣");
        let (_, created) = spaces
            .save("default", "habits", "習慣トラッカー", "<p>v2</p>", None)
            .unwrap();
        assert!(!created);
        let (info, html, data) = spaces.read("default", "habits").unwrap().unwrap();
        assert_eq!(
            (info.title.as_str(), html.as_str()),
            ("習慣トラッカー", "<p>v2</p>")
        );
        assert_eq!(data, json!({"days": 3}));
        spaces
            .set_data("default", "habits", &json!({"days": 4}))
            .unwrap();
        assert_eq!(
            spaces.read("default", "habits").unwrap().unwrap().2,
            json!({"days": 4})
        );
        assert!(spaces.set_data("default", "missing", &json!(1)).is_err());
        assert!(spaces
            .set_data("default", "habits", &json!("x".repeat(MAX_DATA_BYTES)))
            .is_err());
        assert!(spaces.save("default", "../x", "t", "<p>", None).is_err());
        assert_eq!(spaces.list("default").unwrap().len(), 1);
        spaces.remove("default", "habits").unwrap();
        assert!(spaces.read("default", "habits").unwrap().is_none());
    }

    #[test]
    fn data_is_given_before_the_pages_scripts_and_cannot_end_its_element() {
        let info = SpaceInfo {
            name: "board".into(),
            title: "Board".into(),
            created_at: 0,
            updated_at: 0,
        };
        let page = render_page(
            &info,
            "<html><HEAD><title>x</title></head><body></body></html>",
            &json!({"note": "</script><script>alert(1)</script>"}),
        );
        assert!(
            page.starts_with("<html><HEAD><script>window.ANO_SPACE = {"),
            "{page}"
        );
        assert!(!page.contains("</script><script>alert(1)"), "{page}");
        assert!(page.contains("<\\/script>"), "{page}");
        let attributes = render_page(
            &info,
            "<!doctype html><html><head lang=\"ja\"><title>x</title></head></html>",
            &Value::Null,
        );
        assert!(
            attributes.starts_with("<!doctype html><html><head lang=\"ja\"><script>"),
            "{attributes}"
        );
        // <header> is not the head.
        let header = render_page(&info, "<header>x</header>", &Value::Null);
        assert!(header.starts_with("<script>"), "{header}");
        let bare = render_page(&info, "<p>hi</p>", &Value::Null);
        assert!(bare.starts_with("<script>window.ANO_SPACE = ") && bare.ends_with("<p>hi</p>"));
    }
}
