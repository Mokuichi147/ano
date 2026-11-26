//! What the integration tests share.

use std::{path::PathBuf, sync::OnceLock};

/// Variables that put ano's OS config and data directories in a temporary
/// home, so the features that are on by default neither read nor write the
/// user's own (their character, memory, or config).
pub fn isolated_env() -> Vec<(&'static str, PathBuf)> {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    let home = HOME
        .get_or_init(|| tempfile::tempdir().expect("create a temporary home"))
        .path();
    vec![
        ("HOME", home.to_path_buf()),
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_DATA_HOME", home.join(".local/share")),
        ("APPDATA", home.join("AppData/Roaming")),
    ]
}
