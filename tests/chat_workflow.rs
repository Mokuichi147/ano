use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};

/// A chat keeps one conversation across turns, sends reasoning settings and
/// project instructions, and reports reasoning summaries as progress.
#[tokio::test]
async fn chat_carries_history_between_turns_with_project_instructions() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let captured = Arc::clone(&captured);
            async move {
                let mut requests = captured.lock().unwrap();
                requests.push(payload);
                let turn = requests.len();
                Json(json!({
                    "id": format!("resp_{turn}"),
                    "status": "completed",
                    "output": [
                        {"type": "reasoning", "id": format!("rs_{turn}"), "encrypted_content": "opaque",
                         "summary": [{"type": "summary_text", "text": format!("thinking about turn {turn}")}]},
                        {"type": "message", "role": "assistant", "phase": "final_answer",
                         "content": [{"type": "output_text", "text": format!("answer {turn}")}]}
                    ]
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("AGENTS.md"),
        "Always answer in haiku.",
    )
    .unwrap();
    // A config.toml belonging to another application must not stop chat.
    std::fs::write(
        workspace.path().join("config.toml"),
        "[other_application]\nproject = 'another-project'\n",
    )
    .unwrap();
    std::fs::create_dir(workspace.path().join(".ano")).unwrap();
    let config = workspace.path().join(".ano/config.toml");
    std::fs::write(&config, "[agent]\nreasoning_summary = 'auto'\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_ano"))
        .current_dir(workspace.path())
        .env("OPENAI_BASE_URL", &endpoint)
        .env("OPENAI_API_KEY", "test-fixture-key")
        .args(["chat", "--reasoning-effort", "high"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(
            "最初の質問\n/usage\n\n/unknown\n次の質問\n/clear\n三番目\n/exit\nnot sent\n"
                .as_bytes(),
        )
        .await
        .unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    server.abort();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stdout.contains("answer 1") && stdout.contains("answer 2"),
        "{stdout}"
    );
    assert!(
        stderr.contains("[reasoning] thinking about turn 1"),
        "{stderr}"
    );
    assert!(stderr.contains("unknown command /unknown"), "{stderr}");
    assert!(stderr.contains("tokens"), "{stderr}");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "/exit must stop before later lines");
    assert_eq!(
        requests[0]["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert!(requests[0]["instructions"]
        .as_str()
        .unwrap()
        .contains("# Project instructions from AGENTS.md\n"));
    assert!(requests[0]["instructions"]
        .as_str()
        .unwrap()
        .contains("\n\nAlways answer in haiku."));

    // The second turn resends the whole first turn, including reasoning.
    let history = requests[1]["input"].as_array().unwrap();
    let texts = history
        .iter()
        .filter_map(|item| {
            item["content"][0]["text"]
                .as_str()
                .or_else(|| item["encrypted_content"].as_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, vec!["最初の質問", "opaque", "answer 1", "次の質問"]);
    assert_eq!(requests[1]["store"], false);

    // /clear starts a new conversation.
    let history = requests[2]["input"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["content"][0]["text"], "三番目");
}

/// Exercise real CLI discovery without a project config or the user's settings.
/// Windows uses known-folder APIs, which cannot be isolated with child env vars.
#[cfg(unix)]
#[tokio::test]
async fn chat_ignores_unrelated_root_config_with_os_settings_or_defaults() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let captured = Arc::clone(&captured);
            async move {
                captured.lock().unwrap().push(payload);
                Json(json!({
                    "id": "resp_discovery", "status": "completed",
                    "output": [{"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "discovery works"}]}]
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let config_home = home.path().join("config");
    #[cfg(target_os = "macos")]
    let user_config = home
        .path()
        .join("Library/Application Support/ano/config.toml");
    #[cfg(not(target_os = "macos"))]
    let user_config = config_home.join("ano/config.toml");
    std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    std::fs::write(
        workspace.path().join("config.toml"),
        "[other_application]\nunknown_setting = true\n",
    )
    .unwrap();

    for os_config_exists in [true, false] {
        if os_config_exists {
            std::fs::write(&user_config, "[agent]\nmodel = 'shared-discovery-model'\n").unwrap();
        } else {
            std::fs::remove_file(&user_config).unwrap();
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_ano"))
            .current_dir(workspace.path())
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", &config_home)
            .env("OPENAI_BASE_URL", &endpoint)
            .env("OPENAI_API_KEY", "test-fixture-key")
            .arg("chat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"hello\n/exit\n").await.unwrap();
        drop(stdin);
        let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("discovery works"));
    }
    server.abort();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], "shared-discovery-model");
    assert_eq!(
        requests[1]["model"],
        ano::config::AppConfig::default().agent.settings.model
    );
}

/// `/compact` replaces the conversation with the latest request and a
/// summary written by the model, on an endpoint without /responses/compact.
#[tokio::test]
async fn chat_compacts_on_request_with_a_summary() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let captured = Arc::clone(&captured);
            async move {
                let mut requests = captured.lock().unwrap();
                requests.push(payload);
                let turn = requests.len();
                Json(json!({
                    "id": format!("resp_{turn}"),
                    "status": "completed",
                    "output": [{"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": format!("answer {turn}")}]}],
                    "usage": {"input_tokens": 8, "output_tokens": 2, "total_tokens": 10}
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let workspace = tempfile::tempdir().unwrap();
    // The built-in defaults, never the user's config in the OS config directory.
    let config = tempfile::NamedTempFile::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ano"))
        .current_dir(workspace.path())
        .arg("--config")
        .arg(config.path())
        .env("OPENAI_BASE_URL", &endpoint)
        .env("OPENAI_API_KEY", "test-fixture-key")
        .arg("chat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all("最初の質問\n/compact\n次の質問\n/usage\n".as_bytes())
        .await
        .unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    server.abort();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("(compacted 2 items"), "{stderr}");
    assert!(stderr.contains("30 tokens"), "{stderr}");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[1]["instructions"]
        .as_str()
        .unwrap()
        .starts_with("You compact the history"));
    assert!(requests[1]["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("## Assistant\nanswer 1"));
    let history = requests[2]["input"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["content"][0]["text"], "最初の質問");
    assert!(history[0]["content"][1]["text"]
        .as_str()
        .unwrap()
        .ends_with("answer 2"));
    assert_eq!(history[1]["content"][0]["text"], "次の質問");
}

/// `/goal` sets a goal and starts working on it; `/goal` shows it and
/// `/goal clear` removes it.
#[tokio::test]
async fn chat_sets_shows_and_clears_a_goal() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let captured = Arc::clone(&captured);
            async move {
                let mut requests = captured.lock().unwrap();
                requests.push(payload);
                let round = requests.len();
                let item = if round == 1 {
                    json!({"type":"function_call","call_id":"goal","name":"task_plan","arguments":json!({
                        "expected_revision":1,"explanation":null,"steps":null,
                        "goal":{"objective":"Fix it","acceptance":[
                            {"id":"c1","description":"A works","status":"met","evidence":"checked A"}]}
                    }).to_string()})
                } else {
                    json!({"type":"message","role":"assistant",
                        "content":[{"type":"output_text","text":format!("answer {round}")}]})
                };
                Json(json!({"id":format!("resp_{round}"),"status":"completed","output":[item]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let workspace = tempfile::tempdir().unwrap();
    // The built-in defaults, never the user's config in the OS config directory.
    let config = tempfile::NamedTempFile::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ano"))
        .current_dir(workspace.path())
        .arg("--config")
        .arg(config.path())
        .env("OPENAI_BASE_URL", &endpoint)
        .env("OPENAI_API_KEY", "test-fixture-key")
        .arg("chat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all("/goal\n/goal   \n/goal Fix it\n/goal\n/goal clear\n/goal\n".as_bytes())
        .await
        .unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    server.abort();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stdout.contains("answer 2"), "{stdout}");
    assert!(stdout.contains("[met] c1: A works — checked A"), "{stdout}");
    assert_eq!(stderr.matches("(no goal;").count(), 3, "{stderr}");

    assert!(
        stderr.contains("Goal (set by the user): Fix it\n  [met] c1: A works"),
        "{stderr}"
    );
    assert!(stderr.contains("(goal cleared)"), "{stderr}");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let input = requests[0]["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert!(input[0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("Goal set by the user"));
}
