//! The agent (`domain` and `application`) must not depend on the layers
//! around it, so that it stays usable without the harness: see
//! docs/architecture.md. `domain` depends on nothing else. Tests may use the
//! adapters.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// `code` without comments, keeping its lines.
fn without_comments(code: &str) -> String {
    code.lines()
        .map(|line| match line.find("//") {
            Some(start) if !line[..start].contains('"') => &line[..start],
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first segment of every path from the crate root in `code`, with its
/// line: `x` of `crate::x::y`, and each `x` of `crate::{x::y, x2}`. A root
/// re-export such as `crate::Agent` counts as its own segment.
fn crate_paths(code: &str) -> Vec<(usize, String)> {
    let line_of = |index: usize| code[..index].matches('\n').count() + 1;
    let identifier = |start: usize| {
        code[start..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>()
    };
    let mut paths = Vec::new();
    for (index, _) in code.match_indices("crate::") {
        let start = index + "crate::".len();
        if !code[start..].starts_with('{') {
            paths.push((line_of(start), identifier(start)));
            continue;
        }
        // Inside the braces, the first segments sit at depth 1, after the
        // opening brace or a comma.
        let mut depth = 0;
        let mut expect_segment = false;
        for (offset, c) in code[start..].char_indices() {
            match c {
                '{' => {
                    depth += 1;
                    expect_segment = depth == 1;
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                ',' if depth == 1 => expect_segment = true,
                c if c.is_whitespace() => {}
                _ if expect_segment => {
                    expect_segment = false;
                    let at = start + offset;
                    paths.push((line_of(at), identifier(at)));
                }
                _ => {}
            }
        }
    }
    paths
}

#[test]
fn the_agent_does_not_depend_on_the_layers_around_it() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    for (layer, allowed) in [
        ("domain", &["domain"][..]),
        ("application", &["domain", "application"][..]),
    ] {
        let mut files = Vec::new();
        rust_files(&root.join(layer), &mut files);
        assert!(!files.is_empty());
        for path in files {
            if path.file_name().is_some_and(|name| name == "tests.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Unit tests sit at the end of a file.
            let code = without_comments(text.split("#[cfg(test)]").next().unwrap_or_default());
            for (line, segment) in crate_paths(&code) {
                if !allowed.contains(&segment.as_str()) {
                    violations.push(format!("{}:{line}: crate::{segment}", path.display()));
                }
            }
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn paths_from_the_crate_root_are_found_however_they_are_written() {
    let code = without_comments(
        "use crate::harness::Harness;\n\
         pub use crate::{\n    domain::plan::{TaskPlan, StepStatus},\n    infrastructure::fs,\n};\n\
         pub(crate) use crate::{application::agent::Agent, config};\n\
         fn f() -> crate::Agent { crate::interface::cli::run() } // crate::ignored\n",
    );
    let segments = crate_paths(&code);
    let names = |line: usize| {
        segments
            .iter()
            .filter(|(at, _)| *at == line)
            .map(|(_, segment)| segment.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(1), ["harness"]);
    assert_eq!(names(3), ["domain"]);
    assert_eq!(names(4), ["infrastructure"]);
    assert_eq!(names(6), ["application", "config"]);
    assert_eq!(names(7), ["Agent", "interface"]);
}
