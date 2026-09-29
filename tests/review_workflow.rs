use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::process::Command;

fn call(id: &str, name: &str, arguments: Value) -> Value {
    json!({"type":"function_call","call_id":id,"name":name,"arguments":arguments.to_string()})
}

fn final_message(text: &str) -> Value {
    json!({"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":text}]})
}

async fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn is_reviewer(request: &Value) -> bool {
    request["instructions"]
        .as_str()
        .is_some_and(|text| text.contains("You are a code reviewer"))
}

/// The output the tool call `call_id` got, from the requests that followed.
fn call_output(requests: &[Value], call_id: &str) -> Value {
    requests
        .iter()
        .flat_map(|request| request["input"].as_array().cloned().unwrap_or_default())
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        .and_then(|item| serde_json::from_str(item["output"].as_str()?).ok())
        .unwrap_or_else(|| panic!("no output for {call_id}"))
}

#[tokio::test]
async fn pushes_need_a_review_of_the_files_as_they_are() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = Arc::clone(&requests);
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(payload): Json<Value>| {
            let captured = Arc::clone(&captured);
            async move {
                let mut requests = captured.lock().unwrap();
                let reviewer = is_reviewer(&payload);
                requests.push(payload);
                let round = requests.iter().filter(|r| is_reviewer(r) == reviewer).count();
                let item = if reviewer {
                    final_message(if round == 1 {
                        "低: README の末尾に改行がありません。"
                    } else {
                        "重大な問題なし"
                    })
                } else {
                    match round {
                        1 => call("search", "tool_search", json!({"query":"workspace_write git_commit_push"})),
                        2 => call("write1", "workspace_write", json!({"path":"README.md","content":"fixed"})),
                        3 => call("push1", "git_commit_push", json!({"files":["README.md"],"message":"Fix README","branch":"fix"})),
                        4 => call("review1", "review_changes", json!({"request":"README の壊れた画像を削除する"})),
                        5 => call("write2", "workspace_write", json!({"path":"README.md","content":"fixed\n"})),
                        6 => call("push2", "git_commit_push", json!({"files":["README.md"],"message":"Fix README","branch":"fix"})),
                        7 => call("review2", "review_changes", json!({"request":"README の壊れた画像を削除する"})),
                        8 => call("push3", "git_commit_push", json!({"files":["README.md"],"message":"Fix README","branch":"fix"})),
                        _ => final_message("push しました"),
                    }
                };
                Json(json!({"id":format!("response_{}", requests.len()),"status":"completed","output":[item]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // A clone of owner/repo whose GitHub URL is rewritten to a local bare
    // repository.
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let origin = root.join("origin.git");
    let seed = root.join("seed");
    let work = root.join("work");
    let github = "https://github.com/owner/repo.git";
    let rewrite = format!("url.{}.insteadOf={github}", origin.display());
    std::fs::create_dir_all(&seed).unwrap();
    git(
        root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            "origin.git",
        ],
    )
    .await;
    git(&seed, &["init", "--quiet", "--initial-branch=main"]).await;
    std::fs::write(seed.join("README.md"), "![broken](x.svg)\n").unwrap();
    git(&seed, &["add", "."]).await;
    git(
        &seed,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "--quiet",
            "-m",
            "init",
        ],
    )
    .await;
    git(
        &seed,
        &["push", "--quiet", origin.to_str().unwrap(), "main"],
    )
    .await;
    git(root, &["-c", &rewrite, "clone", "--quiet", github, "work"]).await;
    let (key, value) = rewrite.split_once('=').unwrap();
    git(&work, &["config", key, value]).await;
    git(&work, &["config", "user.name", "Test"]).await;
    git(&work, &["config", "user.email", "test@example.com"]).await;
    std::fs::write(
        root.join("agent.toml"),
        "[api]\nmax_retries = 0\n[agent]\nmax_tool_rounds = 12\n",
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_ano"));
    command
        .current_dir(root)
        .env("OPENAI_BASE_URL", &endpoint)
        .env("OPENAI_API_KEY", "mock-key")
        .args(["--config", "agent.toml", "run", "--workspace"])
        .arg(&work)
        .args([
            "--allow-writes",
            "--approval-mode",
            "allow",
            "--json",
            "--quiet",
            "README を直して push して",
        ])
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = requests.lock().unwrap().clone();
    // No review yet: the push is refused before approval.
    let refused = call_output(&requests, "push1");
    assert_eq!(refused["error"], "review_required", "{refused}");
    assert!(refused["message"]
        .as_str()
        .unwrap()
        .contains("not been reviewed"));

    // The reviewer works in a fresh, read-only conversation on the diff.
    let reviewers: Vec<&Value> = requests.iter().filter(|r| is_reviewer(r)).collect();
    assert_eq!(reviewers.len(), 2);
    let task = reviewers[0]["input"].to_string();
    assert!(task.contains("README の壊れた画像を削除する"), "{task}");
    assert!(task.contains("+fixed"), "{task}");
    // It does not see the implementing conversation.
    assert!(!task.contains("write1"), "{task}");
    let offered: Vec<&str> = reviewers[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(!offered.contains(&"review_changes") && !offered.contains(&"delegate_task"));
    let review = call_output(&requests, "review1");
    assert_eq!(review["report"], "低: README の末尾に改行がありません。");
    assert_eq!(review["recorded"], true);
    assert_eq!(review["reviewed_files"], json!(["README.md"]));

    // A change after the review needs another review.
    let changed = call_output(&requests, "push2");
    assert_eq!(changed["error"], "review_required", "{changed}");
    assert!(changed["message"]
        .as_str()
        .unwrap()
        .contains("changed after the last review"));

    let pushed = call_output(&requests, "push3");
    assert_eq!(pushed["branch"], "fix", "{pushed}");
    assert_eq!(git(&origin, &["show", "fix:README.md"]).await, "fixed\n");
}
