//! The character kept as a `SOUL.md` file, and the `character_update` tool.
//!
//! Each user has one file under the platform's data directory (for example
//! `~/Library/Application Support/ano/character/<user>/SOUL.md` on macOS),
//! so the character is the same in every project, and webhook users never
//! share one. The file is Markdown with YAML frontmatter, so it can be
//! written by hand as well.

use crate::{
    application::registry::ToolRegistry,
    domain::{
        character::{Character, CharacterUpdate, CHARACTER_UPDATE_NAME},
        tool::ToolDefinition,
    },
    infrastructure::{
        frontmatter::split_frontmatter,
        fs::{atomic_write, user_directory},
    },
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{io::ErrorKind, path::PathBuf, sync::Arc};

const SOUL_FILE: &str = "SOUL.md";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CharacterSettings {
    pub enabled: bool,
    /// Where characters are kept. Defaults to `character` in the platform's
    /// data directory for ano.
    pub dir: Option<PathBuf>,
}

impl CharacterSettings {
    pub fn directory(&self) -> Result<PathBuf> {
        match &self.dir {
            Some(dir) => Ok(dir.clone()),
            None => default_character_dir().context(
                "cannot determine the data directory for the character; set character.dir",
            ),
        }
    }
}

/// `character` in the platform's data directory: `$XDG_DATA_HOME/ano` (or
/// `~/.local/share/ano`) on Linux, `~/Library/Application Support/ano` on
/// macOS, and `%APPDATA%\ano\data` on Windows.
pub fn default_character_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ano").map(|dirs| dirs.data_dir().join("character"))
}

#[derive(Debug)]
pub struct SavedCharacter {
    pub path: PathBuf,
    /// No character existed before.
    pub created: bool,
}

pub struct CharacterStore {
    root: PathBuf,
}

impl CharacterStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The store when the character is enabled, with its tool registered.
    pub fn from_settings(
        settings: &CharacterSettings,
        registry: &ToolRegistry,
    ) -> Result<Option<Arc<Self>>> {
        if !settings.enabled {
            return Ok(None);
        }
        let store = Arc::new(Self::new(settings.directory()?));
        store.register_tools(registry)?;
        Ok(Some(store))
    }

    /// The file that holds `user`'s character.
    pub fn path(&self, user: &str) -> Result<PathBuf> {
        Ok(user_directory(&self.root, user)?.join(SOUL_FILE))
    }

    /// `user`'s character, or `None` when there is none yet.
    pub fn read(&self, user: &str) -> Result<Option<Character>> {
        let path = self.path(user)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        parse_soul_file(&text)
            .map(Some)
            .with_context(|| format!("invalid character file {}", path.display()))
    }

    /// Save `character` for `user`, replacing the current one in one rename.
    pub fn save(&self, user: &str, character: &Character) -> Result<SavedCharacter> {
        let path = self.path(user)?;
        let directory = path.parent().context("character file has no directory")?;
        std::fs::create_dir_all(directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
        let created = !path.exists();
        atomic_write(&path, render_soul_file(character)?.as_bytes(), None)?;
        Ok(SavedCharacter { path, created })
    }

    /// Apply `update` to `user`'s character and save the result.
    pub fn update(
        &self,
        user: &str,
        update: &CharacterUpdate,
    ) -> Result<(Character, SavedCharacter)> {
        let character = update.apply(self.read(user)?.as_ref())?;
        let saved = self.save(user, &character)?;
        Ok((character, saved))
    }

    pub fn register_tools(self: &Arc<Self>, registry: &ToolRegistry) -> Result<()> {
        if registry.is_registered(CHARACTER_UPDATE_NAME) {
            return Ok(());
        }
        let store = Arc::clone(self);
        registry.register_contextual(
            ToolDefinition::new(
                CHARACTER_UPDATE_NAME,
                "Create your character or change part of it: your name, a one-line tagline, or your soul (personality, tone, and manner of speaking). Fields left out stay as they are. Requires approval.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "The name you go by. Required when no character exists yet."},
                        "tagline": {"type": "string", "description": "One line that sums the character up. An empty string clears it."},
                        "soul": {"type": "string", "description": "The whole new soul in Markdown: personality, tone, and manner of speaking, at most 4 KiB. Read the current one in the instructions and rewrite it rather than appending. An empty string clears it."}
                    },
                    "additionalProperties": false
                }),
            )
            .with_approval()
            .always_offered(),
            move |arguments, context| {
                let store = Arc::clone(&store);
                async move {
                    tokio::task::spawn_blocking(move || {
                        store.update_tool(&context.user_id, &arguments)
                    })
                    .await
                    .context("character_update task failed")?
                }
            },
        )
    }

    fn update_tool(&self, user: &str, arguments: &Value) -> Result<Value> {
        let field = |key: &str| -> Result<Option<String>> {
            match &arguments[key] {
                Value::Null => Ok(None),
                Value::String(text) => Ok(Some(text.clone())),
                _ => bail!("{CHARACTER_UPDATE_NAME}.{key} must be a string"),
            }
        };
        let update = CharacterUpdate {
            name: field("name")?,
            tagline: field("tagline")?,
            soul: field("soul")?,
        };
        let (character, saved) = self.update(user, &update)?;
        Ok(json!({
            "status": if saved.created { "created" } else { "updated" },
            "character": character,
            "note": "The new character applies from now on in this conversation, and to later runs.",
        }))
    }
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    #[serde(default)]
    tagline: String,
}

#[derive(Serialize)]
struct FrontmatterOut<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    tagline: &'a str,
}

/// Read a `SOUL.md`: YAML frontmatter with `name` and an optional `tagline`
/// between `---` lines, then the soul in Markdown. Other frontmatter fields
/// are accepted and ignored.
fn parse_soul_file(text: &str) -> Result<Character> {
    let (frontmatter, soul) = split_frontmatter(text, SOUL_FILE)?;
    let frontmatter: Frontmatter = serde_yaml_ng::from_str(&frontmatter)
        .context("the frontmatter needs a string field name (and optionally tagline)")?;
    Character::new(&frontmatter.name, &frontmatter.tagline, &soul)
}

fn render_soul_file(character: &Character) -> Result<String> {
    let frontmatter = serde_yaml_ng::to_string(&FrontmatterOut {
        name: &character.name,
        tagline: &character.tagline,
    })?;
    Ok(format!("---\n{frontmatter}---\n\n{}\n", character.soul))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn characters_round_trip_through_soul_md() {
        let root = tempfile::tempdir().unwrap();
        let store = CharacterStore::new(root.path());
        assert_eq!(store.read("default").unwrap(), None);

        let character = Character::new("ミラ", "好奇心旺盛な相棒", "# 口調\n明るく話す。").unwrap();
        let saved = store.save("default", &character).unwrap();
        assert!(saved.created);
        assert_eq!(saved.path, root.path().join("default/SOUL.md"));
        let text = std::fs::read_to_string(&saved.path).unwrap();
        assert!(
            text.starts_with("---\nname: ミラ\ntagline: 好奇心旺盛な相棒\n---\n"),
            "{text}"
        );
        assert_eq!(store.read("default").unwrap(), Some(character));
        assert_eq!(store.read("other").unwrap(), None);

        let plain = Character::new("Mira", "", "").unwrap();
        assert!(!store.save("default", &plain).unwrap().created);
        let text = std::fs::read_to_string(&saved.path).unwrap();
        assert_eq!(text, "---\nname: Mira\n---\n\n\n");
        assert_eq!(store.read("default").unwrap(), Some(plain));
    }

    #[test]
    fn hand_written_files_may_carry_other_fields() {
        let parsed = parse_soul_file(
            "\u{feff}---\r\nname: Mira\r\navatar: hippo\r\n---\r\nCalm and brief.\r\n",
        )
        .unwrap();
        assert_eq!(
            parsed,
            Character::new("Mira", "", "Calm and brief.").unwrap()
        );
        for broken in [
            "name: Mira\n",
            "---\nname: Mira\n",
            "---\ntagline: no name\n---\n",
            "---\nname: \"\"\n---\n",
        ] {
            assert!(parse_soul_file(broken).is_err(), "{broken:?}");
        }
    }

    #[test]
    fn the_tool_merges_changes_and_needs_a_name_first() {
        let root = tempfile::tempdir().unwrap();
        let store = CharacterStore::new(root.path());
        assert!(store
            .update_tool("default", &json!({"soul": "Calm."}))
            .is_err());
        assert!(store.update_tool("default", &json!({"name": 3})).is_err());

        let created = store
            .update_tool("default", &json!({"name": "Mira", "soul": "Calm."}))
            .unwrap();
        assert_eq!(created["status"], "created");
        let updated = store
            .update_tool("default", &json!({"tagline": "A partner."}))
            .unwrap();
        assert_eq!(updated["status"], "updated");
        assert_eq!(
            store.read("default").unwrap(),
            Some(Character::new("Mira", "A partner.", "Calm.").unwrap())
        );
    }

    #[test]
    fn user_ids_cannot_leave_the_character_directory() {
        let store = CharacterStore::new("/character");
        assert_eq!(
            store.path("alice@example.com").unwrap(),
            Path::new("/character/alice@example.com/SOUL.md")
        );
        for user in ["", ".", "..", "a/b", "a\\b", "c:", "a\nb"] {
            assert!(store.path(user).is_err(), "{user:?}");
        }
    }
}
