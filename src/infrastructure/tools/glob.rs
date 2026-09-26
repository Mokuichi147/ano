//! Minimal glob patterns for workspace paths.
//!
//! - `*` matches any characters except `/`, `?` matches one character.
//! - `**` as a whole segment matches zero or more directories.
//! - `{a,b}` matches either alternative (not nested).
//! - A pattern without `/` matches the file name at any depth, so `*.rs`
//!   finds Rust files in every directory.

use anyhow::{bail, Result};

const MAX_ALTERNATIVES: usize = 64;

#[derive(Debug, Clone)]
pub(super) struct Glob {
    /// One entry per expanded `{a,b}` alternative, split into segments.
    alternatives: Vec<Vec<String>>,
    name_only: bool,
}

impl Glob {
    pub fn new(pattern: &str) -> Result<Self> {
        let pattern = pattern.trim().trim_start_matches("./");
        if pattern.is_empty() || pattern.len() > 1000 {
            bail!("glob pattern must contain 1 to 1000 characters");
        }
        if pattern.starts_with('/') || pattern.split('/').any(|segment| segment == "..") {
            bail!("glob pattern must be relative and must not contain '..'");
        }
        let alternatives = expand_braces(pattern)?
            .into_iter()
            .map(|pattern| pattern.split('/').map(str::to_string).collect())
            .collect();
        Ok(Self {
            alternatives,
            name_only: !pattern.contains('/'),
        })
    }

    /// Match a `/`-separated path relative to the search root.
    pub fn matches(&self, path: &str) -> bool {
        let segments = path.split('/').collect::<Vec<_>>();
        let target: &[&str] = if self.name_only {
            &segments[segments.len() - 1..]
        } else {
            &segments
        };
        self.alternatives
            .iter()
            .any(|pattern| match_segments(pattern, target))
    }
}

fn expand_braces(pattern: &str) -> Result<Vec<String>> {
    let Some(open) = pattern.find('{') else {
        if pattern.contains('}') {
            bail!("unbalanced '}}' in glob pattern");
        }
        return Ok(vec![pattern.to_string()]);
    };
    let close = pattern[open..]
        .find('}')
        .map(|offset| open + offset)
        .ok_or_else(|| anyhow::anyhow!("unbalanced '{{' in glob pattern"))?;
    let inner = &pattern[open + 1..close];
    if inner.contains('{') {
        bail!("nested braces are not supported in glob patterns");
    }
    let mut expanded = Vec::new();
    for rest in expand_braces(&pattern[close + 1..])? {
        for choice in inner.split(',') {
            expanded.push(format!("{}{choice}{rest}", &pattern[..open]));
            if expanded.len() > MAX_ALTERNATIVES {
                bail!("glob pattern expands to more than {MAX_ALTERNATIVES} alternatives");
            }
        }
    }
    Ok(expanded)
}

fn match_segments(pattern: &[String], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first == "**" => {
            (0..=path.len()).any(|skip| match_segments(rest, &path[skip..]))
        }
        Some((first, rest)) => path.split_first().is_some_and(|(segment, remaining)| {
            match_segment(first, segment) && match_segments(rest, remaining)
        }),
    }
}

fn match_segment(pattern: &str, text: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    // Iterative wildcard matching with backtracking to the last `*`.
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|character| *character == '*')
}

#[cfg(test)]
mod tests {
    use super::Glob;

    fn matches(pattern: &str, path: &str) -> bool {
        Glob::new(pattern).unwrap().matches(path)
    }

    #[test]
    fn name_patterns_match_at_any_depth() {
        assert!(matches("*.rs", "main.rs"));
        assert!(matches("*.rs", "src/domain/plan.rs"));
        assert!(!matches("*.rs", "src/lib.rsx"));
        assert!(matches("Cargo.???l", "crates/a/Cargo.toml"));
    }

    #[test]
    fn path_patterns_respect_directories() {
        assert!(matches("src/*.rs", "src/lib.rs"));
        assert!(!matches("src/*.rs", "src/domain/plan.rs"));
        assert!(matches("src/**/*.rs", "src/lib.rs"));
        assert!(matches("src/**/*.rs", "src/domain/plan.rs"));
        assert!(matches("**/tests/*.rs", "tests/cli.rs"));
        assert!(!matches("docs/*", "src/docs/a.md"));
    }

    #[test]
    fn braces_expand_to_alternatives() {
        assert!(matches("*.{rs,toml}", "Cargo.toml"));
        assert!(matches("*.{rs,toml}", "src/lib.rs"));
        assert!(!matches("*.{rs,toml}", "README.md"));
        assert!(Glob::new("*.{rs").is_err());
        assert!(Glob::new("../*.rs").is_err());
        assert!(Glob::new("/etc/*").is_err());
    }
}
