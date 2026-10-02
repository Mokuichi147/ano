use serde_json::{json, Value};
use std::process::{Command, Output};

fn run(directory: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ano"))
        .current_dir(directory)
        // Keep the user's .env in the OS config directory from setting them again.
        .env("HOME", directory)
        .env("XDG_CONFIG_HOME", directory)
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_BASE_URL")
        .args(["--config", "settings/ano.toml"])
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn auth_status_and_logout_use_config_relative_path_without_api_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir(root.join("settings")).unwrap();
    std::fs::write(
        root.join("settings/ano.toml"),
        "[api]\nauth = 'chatgpt'\nchatgpt_auth_file = 'auth.json'\n",
    )
    .unwrap();
    let missing = run(root, &["auth", "status", "--json"]);
    assert!(
        missing.status.success(),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    let status: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(status, json!({"logged_in":false, "needs_refresh":false}));

    let task = run(root, &["run", "--quiet", "hello"]);
    assert!(!task.status.success());
    assert!(String::from_utf8_lossy(&task.stderr).contains("ano auth login"));

    let path = root.join("settings/auth.json");
    std::fs::write(
        &path,
        json!({"access_token":"secret-access", "refresh_token":"secret-refresh",
        "account_id":"account", "expires_at":0})
        .to_string(),
    )
    .unwrap();
    let output = run(root, &["auth", "status", "--json"]);
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"logged_in":true,"needs_refresh":true})
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("secret"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret"));

    assert!(run(root, &["auth", "logout"]).status.success());
    assert!(!path.exists());
    assert!(run(root, &["auth", "logout"]).status.success());
}

#[test]
fn config_keeps_api_key_auth_as_the_default_and_rejects_unknown_modes() {
    let settings = ano::AppConfig::parse("[api]\nstream = false").unwrap();
    assert_eq!(settings.api.auth, ano::ApiAuth::ApiKey);
    assert!(!settings.api.stream);
    assert!(ano::AppConfig::parse("[api]\nauth = 'unknown'").is_err());
}
