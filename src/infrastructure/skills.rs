//! Skills saved as `SKILL.md` files in the Agent Skills format, and the
//! `skill_read` / `skill_save` tools.
//!
//! Each user has a directory of skills under the platform's data directory
//! (for example `~/Library/Application Support/ano/skills/<user>` on macOS),
//! so skills learned in one project help in others, and webhook users never
//! see each other's skills.

use crate::{
    application::registry::ToolRegistry,
    domain::{
        skill::{validate_skill_name, Skill, SKILL_READ_NAME, SKILL_SAVE_NAME},
        tool::ToolDefinition,
    },
    infrastructure::fs::atomic_write,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};

const SKILL_FILE: &str = "SKILL.md";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillSettings {
    pub enabled: bool,
    /// Where skills are kept. Defaults to `skills` in the platform's data
    /// directory for ano.
    pub dir: Option<PathBuf>,
}

impl SkillSettings {
    pub fn directory(&self) -> Result<PathBuf> {
        match &self.dir {
            Some(dir) => Ok(dir.clone()),
            None => default_skills_dir()
                .context("cannot determine the data directory for skills; set skills.dir"),
        }
    }
}

/// `skills` in the platform's data directory: `$XDG_DATA_HOME/ano` (or
/// `~/.local/share/ano`) on Linux, `~/Library/Application Support/ano` on
/// macOS, and `%APPDATA%\ano\data` on Windows.
pub fn default_skills_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ano").map(|dirs| dirs.data_dir().join("skills"))
}

/// Skills whose files could be read, and a description of each that could not.
#[derive(Debug, Default)]
pub struct SkillListing {
    pub skills: Vec<Skill>,
    pub problems: Vec<String>,
}

#[derive(Debug)]
pub struct SavedSkill {
    pub path: PathBuf,
    /// No skill of that name existed before.
    pub created: bool,
}

pub struct SkillLibrary {
    root: PathBuf,
}

impl SkillLibrary {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The library when skills are enabled, with its tools registered.
    pub fn from_settings(
        settings: &SkillSettings,
        registry: &ToolRegistry,
    ) -> Result<Option<Arc<Self>>> {
        if !settings.enabled {
            return Ok(None);
        }
        let library = Arc::new(Self::new(settings.directory()?));
        library.register_tools(registry)?;
        Ok(Some(library))
    }

    /// The directory that holds `user`'s skills.
    pub fn user_dir(&self, user: &str) -> Result<PathBuf> {
        if user.is_empty()
            || user == "."
            || user == ".."
            || user.chars().any(|character| {
                character.is_control()
                    || matches!(
                        character,
                        '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                    )
            })
        {
            bail!("user id {user:?} cannot be used as a skill directory name");
        }
        Ok(self.root.join(user))
    }

    /// Every skill of `user`, sorted by name. A missing directory means no
    /// skills. Unreadable or invalid files are reported and left out, so one
    /// broken skill does not stop a run.
    pub fn list(&self, user: &str) -> Result<SkillListing> {
        let directory = self.user_dir(user)?;
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(SkillListing::default()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read skill directory {}", directory.display())
                })
            }
        };
        let mut listing = SkillListing::default();
        for entry in entries {
            let entry = entry.with_context(|| {
                format!("failed to read skill directory {}", directory.display())
            })?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') || !entry.path().is_dir() {
                continue;
            }
            match self.read(user, &name) {
                Ok(Some(skill)) => listing.skills.push(skill),
                Ok(None) => {}
                Err(error) => listing.problems.push(format!("{name}: {error:#}")),
            }
        }
        listing.skills.sort_by(|a, b| a.name.cmp(&b.name));
        listing.problems.sort();
        Ok(listing)
    }

    /// The skill called `name`, or `None` when it does not exist.
    pub fn read(&self, user: &str, name: &str) -> Result<Option<Skill>> {
        validate_skill_name(name)?;
        let path = self.user_dir(user)?.join(name).join(SKILL_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()))
            }
        };
        let skill = parse_skill_file(&text)
            .with_context(|| format!("invalid skill file {}", path.display()))?;
        if skill.name != name {
            bail!(
                "{} is named {:?}; the name must match its directory",
                path.display(),
                skill.name
            );
        }
        Ok(Some(skill))
    }

    /// Save `skill`, replacing any skill of the same name in one rename.
    pub fn save(&self, user: &str, skill: &Skill) -> Result<SavedSkill> {
        let directory = self.user_dir(user)?.join(&skill.name);
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
        let path = directory.join(SKILL_FILE);
        let created = !path.exists();
        atomic_write(&path, render_skill_file(skill)?.as_bytes(), None)?;
        Ok(SavedSkill { path, created })
    }

    pub fn register_tools(self: &Arc<Self>, registry: &ToolRegistry) -> Result<()> {
        if !registry.is_registered(SKILL_READ_NAME) {
            let library = Arc::clone(self);
            registry.register_contextual(
                ToolDefinition::new(
                    SKILL_READ_NAME,
                    "Read the full procedure of a saved skill. Saved skills are listed by name in the instructions.",
                    json!({
                        "type": "object",
                        "properties": {"name": {"type": "string", "description": "The skill's name, such as release-build."}},
                        "required": ["name"],
                        "additionalProperties": false
                    }),
                )
                .always_offered(),
                move |arguments, context| {
                    let library = Arc::clone(&library);
                    async move {
                        tokio::task::spawn_blocking(move || {
                            library.read_tool(&context.user_id, &arguments)
                        })
                        .await
                        .context("skill_read task failed")?
                    }
                },
            )?;
        }
        if !registry.is_registered(SKILL_SAVE_NAME) {
            let library = Arc::clone(self);
            registry.register_contextual(
                ToolDefinition::new(
                    SKILL_SAVE_NAME,
                    "Save an approach that worked as a skill for later runs, or replace the skill with the same name. Requires approval.",
                    json!({
                        "type": "object",
                        "properties": {
                            "name": {"type": "string", "description": "Lowercase letters, digits, and hyphens, such as release-build. Reuse an existing name to update that skill."},
                            "description": {"type": "string", "description": "One sentence on what the skill does and when to use it. Later runs see only this until they read the skill."},
                            "body": {"type": "string", "description": "Markdown with the steps, commands, checks, and pitfalls, written for similar tasks rather than only this one."}
                        },
                        "required": ["name", "description", "body"],
                        "additionalProperties": false
                    }),
                )
                .with_approval()
                .always_offered(),
                move |arguments, context| {
                    let library = Arc::clone(&library);
                    async move {
                        tokio::task::spawn_blocking(move || {
                            library.save_tool(&context.user_id, &arguments)
                        })
                        .await
                        .context("skill_save task failed")?
                    }
                },
            )?;
        }
        Ok(())
    }

    fn read_tool(&self, user: &str, arguments: &Value) -> Result<Value> {
        let name = arguments["name"]
            .as_str()
            .context("skill_read.name must be a string")?;
        match self.read(user, name)? {
            Some(skill) => Ok(json!(skill)),
            None => {
                let names = self
                    .list(user)?
                    .skills
                    .into_iter()
                    .map(|skill| skill.name)
                    .collect::<Vec<_>>();
                bail!(
                    "skill {name:?} does not exist; saved skills: {}",
                    if names.is_empty() {
                        "(none)".to_string()
                    } else {
                        names.join(", ")
                    }
                )
            }
        }
    }

    fn save_tool(&self, user: &str, arguments: &Value) -> Result<Value> {
        let field = |key: &str| {
            arguments[key]
                .as_str()
                .with_context(|| format!("skill_save.{key} must be a string"))
        };
        let skill = Skill::new(field("name")?, field("description")?, field("body")?)?;
        let saved = self.save(user, &skill)?;
        Ok(json!({
            "name": skill.name,
            "status": if saved.created { "created" } else { "updated" },
            "path": saved.path,
        }))
    }
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}

#[derive(Serialize)]
struct FrontmatterOut<'a> {
    name: &'a str,
    description: &'a str,
}

/// Read a `SKILL.md`: YAML frontmatter with `name` and `description`
/// between `---` lines, then the Markdown body. Other frontmatter fields of
/// the Agent Skills format are accepted and ignored.
fn parse_skill_file(text: &str) -> Result<Skill> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        bail!("{SKILL_FILE} must start with a --- line and YAML frontmatter");
    }
    let mut frontmatter = String::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        frontmatter.push_str(line);
    }
    if !closed {
        bail!("the frontmatter of {SKILL_FILE} is not closed with a --- line");
    }
    let body: String = lines.collect();
    let frontmatter: Frontmatter = serde_yaml_ng::from_str(&frontmatter)
        .context("the frontmatter needs string fields name and description")?;
    Skill::new(&frontmatter.name, &frontmatter.description, &body)
}

fn render_skill_file(skill: &Skill) -> Result<String> {
    let frontmatter = serde_yaml_ng::to_string(&FrontmatterOut {
        name: &skill.name,
        description: &skill.description,
    })?;
    Ok(format!("---\n{frontmatter}---\n\n{}\n", skill.body))
}

/// Show `path` relative to the home directory when it is inside it.
pub fn display_path(path: &Path) -> String {
    match std::env::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf))
    {
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str) -> Skill {
        Skill::new(
            name,
            &format!("Use for {name}: it says so."),
            "1. Build.\n2. Test.",
        )
        .unwrap()
    }

    #[test]
    fn saved_skills_round_trip_through_skill_md() {
        let root = tempfile::tempdir().unwrap();
        let library = SkillLibrary::new(root.path());
        let saved = library.save("default", &skill("release-build")).unwrap();
        assert!(saved.created);
        assert_eq!(
            saved.path,
            root.path().join("default/release-build/SKILL.md")
        );
        let text = std::fs::read_to_string(&saved.path).unwrap();
        assert!(text.starts_with("---\nname: release-build\n"), "{text}");
        assert_eq!(
            library.read("default", "release-build").unwrap(),
            Some(skill("release-build"))
        );

        let updated = Skill::new("release-build", "Newer.", "Only this.").unwrap();
        assert!(!library.save("default", &updated).unwrap().created);
        assert_eq!(
            library.read("default", "release-build").unwrap(),
            Some(updated)
        );
        assert_eq!(library.read("default", "missing").unwrap(), None);
        assert!(library.list("other").unwrap().skills.is_empty());
    }

    #[test]
    fn hand_written_files_may_carry_other_fields_and_multiline_yaml() {
        let parsed = parse_skill_file(
            "---\r\nname: pdf-forms\r\ndescription: >\r\n  Fill PDF forms\r\n  with pdftk.\r\nlicense: MIT\r\n---\r\n# Steps\r\nRun pdftk.\r\n",
        )
        .unwrap();
        assert_eq!(parsed.name, "pdf-forms");
        assert_eq!(parsed.description, "Fill PDF forms with pdftk.");
        assert_eq!(parsed.body, "# Steps\r\nRun pdftk.");
        for broken in [
            "name: x\n",
            "---\nname: x\ndescription: y\n",
            "---\nname: x\n---\nbody",
            "---\nname: Bad Name\ndescription: y\n---\nbody",
        ] {
            assert!(parse_skill_file(broken).is_err(), "{broken:?}");
        }
    }

    #[test]
    fn listing_reports_broken_skills_without_failing() {
        let root = tempfile::tempdir().unwrap();
        let library = SkillLibrary::new(root.path());
        library.save("default", &skill("b-skill")).unwrap();
        library.save("default", &skill("a-skill")).unwrap();
        let user = root.path().join("default");
        std::fs::create_dir(user.join("broken")).unwrap();
        std::fs::write(user.join("broken/SKILL.md"), "no frontmatter").unwrap();
        std::fs::create_dir(user.join("renamed")).unwrap();
        std::fs::write(
            user.join("renamed/SKILL.md"),
            render_skill_file(&skill("other-name")).unwrap(),
        )
        .unwrap();
        std::fs::create_dir(user.join("empty")).unwrap();
        std::fs::write(user.join("notes.txt"), "not a skill").unwrap();

        let listing = library.list("default").unwrap();
        let names: Vec<_> = listing.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a-skill", "b-skill"]);
        assert_eq!(listing.problems.len(), 2, "{:?}", listing.problems);
        assert!(listing.problems[0].starts_with("broken: "));
        assert!(listing.problems[1].contains("must match its directory"));
    }

    #[test]
    fn user_ids_and_names_cannot_leave_the_skill_directory() {
        let library = SkillLibrary::new("/skills");
        assert_eq!(
            library.user_dir("alice@example.com").unwrap(),
            Path::new("/skills/alice@example.com")
        );
        for user in ["", ".", "..", "a/b", "a\\b", "c:", "a\nb"] {
            assert!(library.user_dir(user).is_err(), "{user:?}");
        }
        assert!(library.read("default", "../secret").is_err());
    }
}
