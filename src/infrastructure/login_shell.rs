//! The environment of the user's login shell, for a process that an app
//! started outside any shell: an editor launched from the Dock starts
//! `ano acp` without what the shell's startup files export, such as the API
//! keys that MCP server URLs refer to.

use anyhow::{bail, Context, Result};
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStrExt,
    time::Duration,
};
use tokio::process::Command;

/// How long the shell may take to start and print its environment.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Printed before the environment, so text the startup files print is
/// skipped.
const MARKER: &[u8] = b"__ANO_LOGIN_SHELL_ENV__\0";
/// Variables that describe the shell process itself, not the user's setup.
const SHELL_OWN: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_"];

/// The variables of the user's login shell (`$SHELL`, as an interactive
/// login shell reads its startup files) that this process lacks, and its
/// `PATH`, which also finds the commands the shell finds.
pub async fn missing_variables() -> Result<Vec<(OsString, OsString)>> {
    let shell = std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| default_shell().into());
    missing_variables_of(&shell).await
}

async fn missing_variables_of(shell: &OsStr) -> Result<Vec<(OsString, OsString)>> {
    let mut command = Command::new(shell);
    command
        .args(["-l", "-i", "-c"])
        .arg(format!(
            "printf '%s\\0' {}; command env -0",
            String::from_utf8_lossy(&MARKER[..MARKER.len() - 1])
        ))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(TIMEOUT, command.output())
        .await
        .with_context(|| format!("{} did not start in time", shell.to_string_lossy()))?
        .with_context(|| format!("failed to start {}", shell.to_string_lossy()))?;
    if !output.status.success() {
        bail!("{} exited with {}", shell.to_string_lossy(), output.status);
    }
    let variables = parse(&output.stdout)
        .with_context(|| format!("{} printed no environment", shell.to_string_lossy()))?;
    Ok(variables
        .into_iter()
        .filter(|(name, _)| name == "PATH" || std::env::var_os(name).is_none())
        .collect())
}

fn default_shell() -> &'static str {
    if cfg!(target_os = "macos") {
        "/bin/zsh"
    } else {
        "/bin/sh"
    }
}

/// The `NAME=VALUE` entries after the marker in `env -0`'s output.
fn parse(output: &[u8]) -> Option<Vec<(OsString, OsString)>> {
    let start = output
        .windows(MARKER.len())
        .position(|window| window == MARKER)?
        + MARKER.len();
    Some(
        output[start..]
            .split(|byte| *byte == 0)
            .filter_map(|entry| {
                let equals = entry.iter().position(|byte| *byte == b'=')?;
                let (name, value) = (&entry[..equals], &entry[equals + 1..]);
                (!name.is_empty() && !SHELL_OWN.contains(&&*String::from_utf8_lossy(name))).then(
                    || {
                        (
                            OsStr::from_bytes(name).to_os_string(),
                            OsStr::from_bytes(value).to_os_string(),
                        )
                    },
                )
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_environment_after_the_marker_counts() {
        let mut output = b"Welcome!\nTOKEN=printed by .zshrc\n".to_vec();
        output.extend_from_slice(MARKER);
        output.extend_from_slice(b"TOKEN=secret\0MULTI=a\nb\0PWD=/tmp\0SHLVL=2\0EMPTY=\0");
        let variables = parse(&output).unwrap();
        assert_eq!(
            variables,
            [
                ("TOKEN".into(), "secret".into()),
                ("MULTI".into(), "a\nb".into()),
                ("EMPTY".into(), "".into()),
            ]
        );
        assert!(parse(b"no marker").is_none());
    }

    #[tokio::test]
    async fn the_shells_variables_fill_in_what_the_process_lacks() {
        let folder = tempfile::tempdir().unwrap();
        let shell = folder.path().join("fake-shell");
        // Like a login shell: startup files print text, then the command
        // runs and prints the environment.
        std::fs::write(
            &shell,
            "#!/bin/sh\necho 'Welcome!'\nexport ANO_TEST_LOGIN_ONLY=from-shell\nexport PATH=/shell/bin:$PATH\nexec /bin/sh -c \"$4\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let variables = missing_variables_of(shell.as_os_str()).await.unwrap();
        let value = |name: &str| {
            variables
                .iter()
                .find(|(each, _)| each == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        assert_eq!(value("ANO_TEST_LOGIN_ONLY").as_deref(), Some("from-shell"));
        assert!(value("PATH").unwrap().starts_with("/shell/bin:"));
        // What the process already has stays as it is.
        assert_eq!(value("HOME"), None);

        std::fs::write(&shell, "#!/bin/sh\necho 'no environment'\n").unwrap();
        assert!(missing_variables_of(shell.as_os_str()).await.is_err());
    }
}
