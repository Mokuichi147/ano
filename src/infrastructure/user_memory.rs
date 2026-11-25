//! Memory kept as a `MEMORY.md` file, and the `memory_save` /
//! `memory_forget` tools.
//!
//! Each user has one file under the platform's data directory (for example
//! `~/Library/Application Support/ano/memory/<user>/MEMORY.md` on macOS).
//! Each fact is a `- ` line, so the file can be read and edited by hand;
//! other lines are kept as they are but are not facts.

use crate::{
    application::registry::ToolRegistry,
    domain::{
        text::cut_within,
        tool::ToolDefinition,
        user_memory::{
            normalize_fact, MAX_FACTS, MAX_FACT_CHARS, MEMORY_FORGET_NAME, MEMORY_SAVE_NAME,
        },
    },
    infrastructure::fs::{atomic_write, lock_in, user_directory},
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{fs::File, io::ErrorKind, path::PathBuf, sync::Arc};

const MEMORY_FILE: &str = "MEMORY.md";
const LOCK_FILE: &str = "MEMORY.md.lock";
const HEADER: &str = "# Memory\n\nFacts about the user that ano keeps. One `- ` line per fact.\n\n";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemorySettings {
    pub enabled: bool,
    /// Where memory is kept. Defaults to `memory` in the platform's data
    /// directory for ano.
    pub dir: Option<PathBuf>,
}

/// Enabled unless the config file turns it off.
impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: None,
        }
    }
}

impl MemorySettings {
    pub fn directory(&self) -> Result<PathBuf> {
        match &self.dir {
            Some(dir) => Ok(dir.clone()),
            None => default_memory_dir()
                .context("cannot determine the data directory for memory; set memory.dir"),
        }
    }
}

/// `memory` in the platform's data directory: `$XDG_DATA_HOME/ano` (or
/// `~/.local/share/ano`) on Linux, `~/Library/Application Support/ano` on
/// macOS, and `%APPDATA%\ano\data` on Windows.
pub fn default_memory_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ano").map(|dirs| dirs.data_dir().join("memory"))
}

/// What a save did.
#[derive(Debug, PartialEq, Eq)]
pub enum Saved {
    Added,
    /// The same fact was already kept.
    Unchanged,
}

pub struct MemoryBook {
    root: PathBuf,
}

impl MemoryBook {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The book when memory is enabled, with its tools registered.
    pub fn from_settings(
        settings: &MemorySettings,
        registry: &ToolRegistry,
    ) -> Result<Option<Arc<Self>>> {
        if !settings.enabled {
            return Ok(None);
        }
        let book = Arc::new(Self::new(settings.directory()?));
        book.register_tools(registry)?;
        Ok(Some(book))
    }

    /// The file that holds `user`'s memory.
    pub fn path(&self, user: &str) -> Result<PathBuf> {
        Ok(user_directory(&self.root, user)?.join(MEMORY_FILE))
    }

    /// `user`'s facts in saved order, oldest first. No file means none.
    pub fn facts(&self, user: &str) -> Result<Vec<String>> {
        Ok(parse_facts(&self.read_text(user)?))
    }

    /// Keep `fact` for `user`, after the facts kept before.
    pub fn save(&self, user: &str, fact: &str) -> Result<Saved> {
        let fact = normalize_fact(fact)?;
        let _lock = self.lock(user)?;
        let text = self.read_text(user)?;
        let facts = parse_facts(&text);
        if facts.iter().any(|kept| kept.eq_ignore_ascii_case(&fact)) {
            return Ok(Saved::Unchanged);
        }
        if facts.len() >= MAX_FACTS {
            bail!("memory already keeps {MAX_FACTS} facts; forget outdated or merge related ones before saving more");
        }
        let mut text = if text.trim().is_empty() {
            HEADER.to_string()
        } else {
            text
        };
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("- {fact}\n"));
        self.write_text(user, &text)?;
        Ok(Saved::Added)
    }

    /// Remove the facts of `user` that match one of `facts` (ignoring
    /// surrounding whitespace and ASCII case). Returns those not found.
    pub fn forget(&self, user: &str, facts: &[String]) -> Result<Vec<String>> {
        let wanted: Vec<String> = facts
            .iter()
            .map(|fact| fact_of(&format!("- {fact}")).unwrap_or_default())
            .collect();
        let _lock = self.lock(user)?;
        let text = self.read_text(user)?;
        let mut found = vec![false; wanted.len()];
        let mut kept = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            if let Some(fact) = fact_of(line) {
                if let Some(index) = wanted
                    .iter()
                    .position(|wanted| wanted.eq_ignore_ascii_case(&fact))
                {
                    found[index] = true;
                    continue;
                }
            }
            kept.push_str(line);
        }
        if found.iter().any(|found| *found) {
            self.write_text(user, &kept)?;
        }
        Ok(wanted
            .into_iter()
            .zip(found)
            .filter(|(_, found)| !found)
            .map(|(fact, _)| fact)
            .collect())
    }

    fn read_text(&self, user: &str) -> Result<String> {
        let path = self.path(user)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn write_text(&self, user: &str, text: &str) -> Result<()> {
        atomic_write(&self.path(user)?, text.as_bytes(), None)
    }

    fn lock(&self, user: &str) -> Result<File> {
        let path = self.path(user)?;
        let directory = path.parent().context("memory file has no directory")?;
        lock_in(directory, LOCK_FILE).with_context(|| {
            format!(
                "memory is being changed by another process: {}",
                path.display()
            )
        })
    }

    pub fn register_tools(self: &Arc<Self>, registry: &ToolRegistry) -> Result<()> {
        if !registry.is_registered(MEMORY_SAVE_NAME) {
            let book = Arc::clone(self);
            registry.register_contextual(
                ToolDefinition::new(
                    MEMORY_SAVE_NAME,
                    "Remember one short fact about the user for later conversations. Requires approval.",
                    json!({
                        "type": "object",
                        "properties": {"fact": {"type": "string", "description": "One fact in one sentence, at most 300 characters, such as \"Prefers concise answers in Japanese.\""}},
                        "required": ["fact"],
                        "additionalProperties": false
                    }),
                )
                .with_approval()
                .always_offered(),
                move |arguments, context| {
                    let book = Arc::clone(&book);
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let fact = arguments["fact"]
                                .as_str()
                                .context("memory_save.fact must be a string")?;
                            let saved = book.save(&context.user_id, fact)?;
                            Ok(json!({"status": match saved {
                                Saved::Added => "saved",
                                Saved::Unchanged => "already saved",
                            }}))
                        })
                        .await
                        .context("memory_save task failed")?
                    }
                },
            )?;
        }
        if !registry.is_registered(MEMORY_FORGET_NAME) {
            let book = Arc::clone(self);
            registry.register_contextual(
                ToolDefinition::new(
                    MEMORY_FORGET_NAME,
                    "Forget remembered facts about the user, given exactly as they appear in the memory list. Requires approval.",
                    json!({
                        "type": "object",
                        "properties": {"facts": {"type": "array", "items": {"type": "string"}, "description": "The facts to forget, copied from the memory list."}},
                        "required": ["facts"],
                        "additionalProperties": false
                    }),
                )
                .with_approval()
                .always_offered(),
                move |arguments, context| {
                    let book = Arc::clone(&book);
                    async move {
                        tokio::task::spawn_blocking(move || book.forget_tool(&context.user_id, &arguments))
                            .await
                            .context("memory_forget task failed")?
                    }
                },
            )?;
        }
        Ok(())
    }

    fn forget_tool(&self, user: &str, arguments: &Value) -> Result<Value> {
        let facts = arguments["facts"]
            .as_array()
            .context("memory_forget.facts must be an array of strings")?
            .iter()
            .map(|fact| {
                fact.as_str()
                    .map(str::to_string)
                    .context("memory_forget.facts must be an array of strings")
            })
            .collect::<Result<Vec<_>>>()?;
        if facts.is_empty() {
            bail!("give at least one fact to forget");
        }
        let missing = self.forget(user, &facts)?;
        if missing.len() == facts.len() {
            bail!("none of these facts is remembered; copy them exactly from the memory list");
        }
        Ok(json!({"forgotten": facts.len() - missing.len(), "not_found": missing}))
    }
}

/// The fact of a `- ` line, normalized as saved facts are. A line written
/// by hand gets the same limits: control characters are dropped and a long
/// line is cut, so one line cannot crowd out the others.
fn fact_of(line: &str) -> Option<String> {
    let fact = line.trim_start().strip_prefix("- ")?;
    let fact: String = fact
        .chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect();
    let fact = fact.split_whitespace().collect::<Vec<_>>().join(" ");
    (!fact.is_empty()).then(|| cut_within(&fact, MAX_FACT_CHARS))
}

fn parse_facts(text: &str) -> Vec<String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.lines().filter_map(fact_of).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_are_appended_kept_once_and_forgotten_by_text() {
        let root = tempfile::tempdir().unwrap();
        let book = MemoryBook::new(root.path());
        assert!(book.facts("default").unwrap().is_empty());

        assert_eq!(
            book.save("default", "Likes green tea.").unwrap(),
            Saved::Added
        );
        assert_eq!(
            book.save("default", " likes  GREEN tea. ").unwrap(),
            Saved::Unchanged
        );
        book.save("default", "Has a cat named Mochi.").unwrap();
        let text = std::fs::read_to_string(book.path("default").unwrap()).unwrap();
        assert!(text.starts_with("# Memory\n"), "{text}");
        assert_eq!(
            book.facts("default").unwrap(),
            ["Likes green tea.", "Has a cat named Mochi."]
        );

        let missing = book
            .forget(
                "default",
                &["likes green tea.".into(), "Never said this.".into()],
            )
            .unwrap();
        assert_eq!(missing, ["Never said this."]);
        assert_eq!(book.facts("default").unwrap(), ["Has a cat named Mochi."]);
        assert!(book.facts("other").unwrap().is_empty());
    }

    #[test]
    fn hand_written_lines_survive_and_only_dash_lines_are_facts() {
        let root = tempfile::tempdir().unwrap();
        let book = MemoryBook::new(root.path());
        let path = book.path("default").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "\u{feff}# My notes\nFree text.\n  - Works  remotely\n-not a fact\n- Early riser",
        )
        .unwrap();
        assert_eq!(
            book.facts("default").unwrap(),
            ["Works remotely", "Early riser"]
        );
        book.save("default", "Plays the piano.").unwrap();
        book.forget("default", &["Works remotely".into()]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "\u{feff}# My notes\nFree text.\n-not a fact\n- Early riser\n- Plays the piano.\n"
        );
    }

    #[test]
    fn long_hand_written_lines_are_cut_and_can_still_be_forgotten() {
        let root = tempfile::tempdir().unwrap();
        let book = MemoryBook::new(root.path());
        let path = book.path("default").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("- Short.\n- {}\u{7}\n", "x".repeat(5000))).unwrap();
        let facts = book.facts("default").unwrap();
        assert_eq!(facts[0], "Short.");
        assert_eq!(facts[1].chars().count(), MAX_FACT_CHARS);
        assert!(facts[1].ends_with('…'));
        let missing = book.forget("default", &[facts[1].clone()]).unwrap();
        assert!(missing.is_empty());
        assert_eq!(book.facts("default").unwrap(), ["Short."]);
    }

    #[test]
    fn saving_stops_at_the_limit() {
        let root = tempfile::tempdir().unwrap();
        let book = MemoryBook::new(root.path());
        let path = book.path("default").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let full: String = (0..MAX_FACTS).map(|n| format!("- fact {n}\n")).collect();
        std::fs::write(&path, full).unwrap();
        assert!(book.save("default", "one more").is_err());
        assert_eq!(book.save("default", "fact 3").unwrap(), Saved::Unchanged);
    }

    #[test]
    fn the_forget_tool_reports_what_it_could_not_find() {
        let root = tempfile::tempdir().unwrap();
        let book = MemoryBook::new(root.path());
        book.save("default", "Likes tea.").unwrap();
        assert!(book
            .forget_tool("default", &json!({"facts": ["Unknown."]}))
            .is_err());
        assert!(book.forget_tool("default", &json!({"facts": []})).is_err());
        let result = book
            .forget_tool("default", &json!({"facts": ["Likes tea.", "Unknown."]}))
            .unwrap();
        assert_eq!(result, json!({"forgotten": 1, "not_found": ["Unknown."]}));
    }
}
