//! Markdown files that start with YAML frontmatter, such as `SKILL.md` and
//! `SOUL.md`.

use anyhow::{bail, Result};

/// Split `text` into the YAML between its leading `---` lines and the
/// Markdown after them. A byte order mark is skipped. `file` names the file
/// in errors.
pub(crate) fn split_frontmatter(text: &str, file: &str) -> Result<(String, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        bail!("{file} must start with a --- line and YAML frontmatter");
    }
    let mut frontmatter = String::new();
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            return Ok((frontmatter, lines.collect()));
        }
        frontmatter.push_str(line);
    }
    bail!("the frontmatter of {file} is not closed with a --- line")
}
