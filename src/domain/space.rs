//! Spaces: small pages the agent builds for the user on request, such as a
//! habit tracker, a weekly planner, or a board of upcoming events, whose
//! data the agent keeps up to date, for example from a scheduled task.
//!
//! A space is one HTML page with its own data. `ano web` shows it isolated
//! from everything else: it runs with no origin and may not connect
//! anywhere, so its scripts can read only the data given to it.

use anyhow::{bail, Result};

/// Tool that creates or replaces a space.
pub const SPACE_SAVE_NAME: &str = "space_save";
/// Tool that reads a space's page and data, to change it.
pub const SPACE_READ_NAME: &str = "space_read";
/// Tool that replaces a space's data.
pub const SPACE_DATA_NAME: &str = "space_data_set";

pub const MAX_NAME_CHARS: usize = 40;
pub const MAX_TITLE_CHARS: usize = 60;
pub const MAX_HTML_BYTES: usize = 200 * 1024;
pub const MAX_DATA_BYTES: usize = 64 * 1024;
/// Spaces one user may keep.
pub const MAX_SPACES: usize = 30;

/// Space names are path components and part of URLs: lowercase ASCII
/// letters, digits, and single hyphens.
pub fn validate_space_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_NAME_CHARS
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
    {
        bail!("space name must be 1 to {MAX_NAME_CHARS} lowercase letters, digits, and single hyphens, such as weekly-planner: {name:?}");
    }
    Ok(())
}

/// A title within the limit, as one line.
pub fn check_title(title: &str) -> Result<String> {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        bail!("title must be 1 to {MAX_TITLE_CHARS} characters");
    }
    Ok(title)
}

pub fn check_html(html: &str) -> Result<()> {
    if html.trim().is_empty() {
        bail!("html must not be empty");
    }
    if html.len() > MAX_HTML_BYTES {
        bail!("html must be at most {} KiB", MAX_HTML_BYTES / 1024);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_titles_and_pages_are_bounded() {
        for name in ["weekly-planner", "a", "habit-2026"] {
            validate_space_name(name).unwrap();
        }
        for name in [
            "",
            "Planner",
            "a_b",
            "-a",
            "a-",
            "a--b",
            "../x",
            &"x".repeat(41),
        ] {
            assert!(validate_space_name(name).is_err(), "{name:?}");
        }
        assert_eq!(
            check_title(" 週間 \n プランナー ").unwrap(),
            "週間 プランナー"
        );
        assert!(check_title(" ").is_err());
        assert!(check_html(" ").is_err());
        assert!(check_html(&"x".repeat(MAX_HTML_BYTES + 1)).is_err());
    }
}
