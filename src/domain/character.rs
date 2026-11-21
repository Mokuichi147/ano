//! The character: the name, one-line tagline, and personality ("soul") the
//! agent takes on when it talks with its user.
//!
//! Each user has at most one character. It shapes how the agent speaks to
//! the user, not what the agent may do: it never overrides the operator's
//! instructions, the user's requests, or the approval rules. The soul is
//! sent with every request, so it is kept short.

use anyhow::{bail, Result};
use serde::Serialize;

/// Tool that creates the character or changes part of it. It always
/// requires approval, because the character shapes every later run.
pub const CHARACTER_UPDATE_NAME: &str = "character_update";

pub const MAX_CHARACTER_NAME_CHARS: usize = 64;
pub const MAX_CHARACTER_TAGLINE_CHARS: usize = 200;
/// Size limit of the soul. It is part of every request's instructions, which
/// matters most for local models with small context windows.
pub const MAX_CHARACTER_SOUL_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Character {
    pub name: String,
    /// One line that sums the character up; may be empty.
    pub tagline: String,
    /// Markdown on personality, tone, and manner of speaking; may be empty.
    pub soul: String,
}

impl Character {
    /// A character with a one-line name, a one-line tagline, and a soul
    /// within the size limit. Surrounding whitespace is trimmed.
    pub fn new(name: &str, tagline: &str, soul: &str) -> Result<Self> {
        let name = name.trim();
        if name.is_empty() {
            bail!("character name must not be empty");
        }
        if name.chars().count() > MAX_CHARACTER_NAME_CHARS {
            bail!("character name must be at most {MAX_CHARACTER_NAME_CHARS} characters");
        }
        if name.chars().any(char::is_control) {
            bail!("character name must be one line without control characters");
        }
        let tagline = tagline.trim();
        if tagline.chars().count() > MAX_CHARACTER_TAGLINE_CHARS {
            bail!("character tagline must be at most {MAX_CHARACTER_TAGLINE_CHARS} characters");
        }
        if tagline.chars().any(char::is_control) {
            bail!("character tagline must be one line without control characters");
        }
        let soul = soul.trim();
        if soul.len() > MAX_CHARACTER_SOUL_BYTES {
            bail!(
                "character soul must be at most {} KiB",
                MAX_CHARACTER_SOUL_BYTES / 1024
            );
        }
        Ok(Self {
            name: name.to_string(),
            tagline: tagline.to_string(),
            soul: soul.to_string(),
        })
    }
}

/// A change to the character: the fields given replace the current ones,
/// and the others stay. An empty tagline or soul clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CharacterUpdate {
    pub name: Option<String>,
    pub tagline: Option<String>,
    pub soul: Option<String>,
}

impl CharacterUpdate {
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.tagline.is_none() && self.soul.is_none()
    }

    /// The character after this change to `current`. Without a current
    /// character, the change must give the name.
    pub fn apply(&self, current: Option<&Character>) -> Result<Character> {
        if self.is_empty() {
            bail!("give at least one of name, tagline, or soul to change");
        }
        let pick = |given: &Option<String>, current: Option<&str>| {
            given.as_deref().or(current).unwrap_or_default().to_string()
        };
        let name = pick(&self.name, current.map(|c| c.name.as_str()));
        if name.trim().is_empty() && current.is_none() {
            bail!("the character has no name yet; give one");
        }
        Character::new(
            &name,
            &pick(&self.tagline, current.map(|c| c.tagline.as_str())),
            &pick(&self.soul, current.map(|c| c.soul.as_str())),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_needs_a_one_line_name_and_a_bounded_soul() {
        let character =
            Character::new("  ミラ \n", " 好奇心旺盛な相棒 ", "\n明るく話す。\n").unwrap();
        assert_eq!(character.name, "ミラ");
        assert_eq!(character.tagline, "好奇心旺盛な相棒");
        assert_eq!(character.soul, "明るく話す。");
        Character::new("Mira", "", "").unwrap();
        for (name, tagline, soul) in [
            (" ", "", ""),
            ("Mi\nra", "", ""),
            ("Mira", "one\ntwo", ""),
            (&"x".repeat(MAX_CHARACTER_NAME_CHARS + 1), "", ""),
            ("Mira", &"t".repeat(MAX_CHARACTER_TAGLINE_CHARS + 1), ""),
            ("Mira", "", &"s".repeat(MAX_CHARACTER_SOUL_BYTES + 1)),
        ] {
            assert!(
                Character::new(name, tagline, soul).is_err(),
                "{name:?} {tagline:?} {}",
                soul.len()
            );
        }
    }

    #[test]
    fn updates_replace_only_the_fields_they_give() {
        let current = Character::new("Mira", "A curious partner.", "Speaks warmly.").unwrap();
        let renamed = CharacterUpdate {
            name: Some("Luna".into()),
            ..Default::default()
        }
        .apply(Some(&current))
        .unwrap();
        assert_eq!(
            renamed,
            Character::new("Luna", "A curious partner.", "Speaks warmly.").unwrap()
        );
        let cleared = CharacterUpdate {
            tagline: Some(String::new()),
            ..Default::default()
        }
        .apply(Some(&current))
        .unwrap();
        assert_eq!(cleared.tagline, "");
        assert_eq!(cleared.soul, "Speaks warmly.");

        assert!(CharacterUpdate::default().apply(Some(&current)).is_err());
        let without_name = CharacterUpdate {
            soul: Some("Calm.".into()),
            ..Default::default()
        };
        assert!(without_name.apply(None).is_err());
        let created = CharacterUpdate {
            name: Some("Mira".into()),
            ..without_name
        }
        .apply(None)
        .unwrap();
        assert_eq!(created, Character::new("Mira", "", "Calm.").unwrap());
        let unnamed = CharacterUpdate {
            name: Some(" ".into()),
            ..Default::default()
        };
        assert!(unnamed.apply(Some(&current)).is_err());
    }
}
