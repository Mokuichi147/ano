//! Project instruction files such as `AGENTS.md` in the workspace root.

use crate::harness::instructions::MAX_PROJECT_INSTRUCTIONS_BYTES;
use anyhow::{bail, Context, Result};
use std::{io::ErrorKind, path::Path};

/// Read the configured instruction files that exist in `workspace`, in order.
///
/// Missing files are skipped. A file that resolves outside the workspace, is
/// not UTF-8, or makes the total exceed the size limit is an error rather
/// than being silently ignored or truncated.
pub fn read_project_instructions(
    workspace: &Path,
    names: &[String],
) -> Result<Vec<(String, String)>> {
    let root = std::fs::canonicalize(workspace)
        .with_context(|| format!("workspace does not exist: {}", workspace.display()))?;
    let mut sources = Vec::new();
    let mut total = 0;
    for name in names {
        let path = match std::fs::canonicalize(root.join(name)) {
            Ok(path) => path,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to resolve project instructions {name}"))
            }
        };
        if !path.starts_with(&root) {
            bail!("project instructions {name} resolve outside the workspace");
        }
        if !path.is_file() {
            bail!("project instructions {name} is not a regular file");
        }
        let bytes = std::fs::read(&path)
            .with_context(|| format!("failed to read project instructions {name}"))?;
        total += bytes.len();
        if total > MAX_PROJECT_INSTRUCTIONS_BYTES {
            bail!(
                "project instructions exceed {} KiB; shorten {name} or remove it from agent.project_instructions",
                MAX_PROJECT_INSTRUCTIONS_BYTES / 1024
            );
        }
        let text = String::from_utf8(bytes)
            .with_context(|| format!("project instructions {name} are not valid UTF-8"))?;
        sources.push((name.clone(), text));
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_existing_files_in_order_and_skips_missing_ones() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("AGENTS.md"), "Use tabs.").unwrap();
        std::fs::create_dir(workspace.path().join("docs")).unwrap();
        std::fs::write(workspace.path().join("docs/rules.md"), "Write tests.").unwrap();
        let sources = read_project_instructions(
            workspace.path(),
            &[
                "docs/rules.md".into(),
                "missing.md".into(),
                "AGENTS.md".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            sources,
            vec![
                ("docs/rules.md".to_string(), "Write tests.".to_string()),
                ("AGENTS.md".to_string(), "Use tabs.".to_string())
            ]
        );
    }

    #[test]
    fn rejects_oversized_files_and_links_outside_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("AGENTS.md"),
            "x".repeat(MAX_PROJECT_INSTRUCTIONS_BYTES + 1),
        )
        .unwrap();
        assert!(read_project_instructions(workspace.path(), &["AGENTS.md".into()]).is_err());

        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret.md"), "secret").unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("secret.md"),
                workspace.path().join("LINK.md"),
            )
            .unwrap();
            assert!(read_project_instructions(workspace.path(), &["LINK.md".into()]).is_err());
        }
    }
}
