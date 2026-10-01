//! The agent (`domain` and `application`) must not depend on the layers
//! around it, so that it stays usable without the harness: see
//! docs/architecture.md. Tests may use the adapters.

use std::path::Path;

const OUTER_LAYERS: &[&str] = &[
    "crate::harness",
    "crate::infrastructure",
    "crate::config",
    "crate::interface",
];

fn rust_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn the_agent_does_not_depend_on_the_layers_around_it() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    for layer in ["domain", "application"] {
        rust_files(&root.join(layer), &mut files);
    }
    assert!(!files.is_empty());
    let mut violations = Vec::new();
    for path in files {
        if path.file_name().is_some_and(|name| name == "tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        // Unit tests sit at the end of a file.
        let code = text.split("#[cfg(test)]").next().unwrap_or_default();
        // Inside `use crate::{ ... }`, the layers are named without `crate::`.
        let mut grouped_use = false;
        for (number, line) in code.lines().enumerate() {
            let line = line.trim_start();
            if line.starts_with("//") {
                continue;
            }
            let in_group = grouped_use || line.starts_with("use crate::{");
            let names_layer = OUTER_LAYERS.iter().any(|layer| {
                let name = layer.trim_start_matches("crate::");
                line.contains(layer)
                    || (in_group
                        && line
                            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
                            .any(|path| path == name || path.starts_with(&format!("{name}::"))))
            });
            if names_layer {
                violations.push(format!("{}:{}: {line}", path.display(), number + 1));
            }
            grouped_use = in_group && !line.ends_with("};");
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}
