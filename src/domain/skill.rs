//! Skills: procedures that worked in earlier tasks, kept so that later runs
//! can follow them instead of finding the approach again.
//!
//! A skill has a short name, a one-line description that tells when it
//! applies, and a Markdown body with the steps. Runs see only the names and
//! descriptions up front and read a body when a task matches it.

use anyhow::{bail, Result};
use serde::Serialize;

/// Tool that returns the body of a saved skill.
pub const SKILL_READ_NAME: &str = "skill_read";
/// Tool that saves or replaces a skill. It always requires approval, because
/// a skill becomes guidance for every later run.
pub const SKILL_SAVE_NAME: &str = "skill_save";

pub const MAX_SKILL_NAME_CHARS: usize = 64;
pub const MAX_SKILL_DESCRIPTION_CHARS: usize = 1024;
/// Size limit of one skill's body, so reading a skill never floods the context.
pub const MAX_SKILL_BODY_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

impl Skill {
    /// A skill with a valid name, a non-empty description, and a body within
    /// the size limit. Surrounding whitespace is trimmed.
    pub fn new(name: &str, description: &str, body: &str) -> Result<Self> {
        validate_skill_name(name)?;
        let description = description.trim();
        if description.is_empty() {
            bail!("skill description must not be empty");
        }
        if description.chars().count() > MAX_SKILL_DESCRIPTION_CHARS {
            bail!("skill description must be at most {MAX_SKILL_DESCRIPTION_CHARS} characters");
        }
        let body = body.trim();
        if body.is_empty() {
            bail!("skill body must not be empty");
        }
        if body.len() > MAX_SKILL_BODY_BYTES {
            bail!(
                "skill body must be at most {} KiB",
                MAX_SKILL_BODY_BYTES / 1024
            );
        }
        Ok(Self {
            name: name.to_string(),
            description: description.to_string(),
            body: body.to_string(),
        })
    }
}

/// Skill names follow the Agent Skills format: 1 to 64 lowercase ASCII
/// letters, digits, and single hyphens, not starting or ending with a
/// hyphen. The name is also the skill's directory name, so this keeps it a
/// plain path component on every platform.
pub fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_SKILL_NAME_CHARS {
        bail!("skill name must be 1 to {MAX_SKILL_NAME_CHARS} characters: {name:?}");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
    {
        bail!("skill name must use lowercase letters, digits, and single hyphens (for example \"release-build\"): {name:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_plain_lowercase_path_components() {
        for name in ["release-build", "a", "rust-2024", &"x".repeat(64)] {
            validate_skill_name(name).unwrap();
        }
        for name in [
            "",
            "Release",
            "release_build",
            "-release",
            "release-",
            "a--b",
            "../etc",
            "リリース",
            &"x".repeat(65),
        ] {
            assert!(validate_skill_name(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn a_skill_needs_a_description_and_a_bounded_body() {
        let skill = Skill::new("build", "  Build the release.\n", "\n1. Run it.\n").unwrap();
        assert_eq!(skill.description, "Build the release.");
        assert_eq!(skill.body, "1. Run it.");
        assert!(Skill::new("build", " ", "steps").is_err());
        assert!(Skill::new("build", "Build.", " ").is_err());
        assert!(Skill::new(
            "build",
            &"d".repeat(MAX_SKILL_DESCRIPTION_CHARS + 1),
            "steps"
        )
        .is_err());
        assert!(Skill::new("build", "Build.", &"s".repeat(MAX_SKILL_BODY_BYTES + 1)).is_err());
    }
}
