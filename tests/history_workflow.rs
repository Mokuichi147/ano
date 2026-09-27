//! 実際の chronotope サーバと CLI を接続する結合テスト。
//! ANO_CHRONOTOPE_BIN にビルド済みの実行ファイルを指定して --ignored で実行する。

use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
};

fn message(text: &str) -> Value {
    json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})
}
fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"type":"function_call","call_id":id,"name":name,"arguments":args.to_string()})
}
fn last_result(payload: &Value) -> Value {
    let item = payload["input"].as_array().unwrap().last().unwrap();
    serde_json::from_str(item["output"].as_str().unwrap()).unwrap()
}
fn free_addr() -> String {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    socket.local_addr().unwrap().to_string()
}
async fn start_database(root: &Path, addr: &str) -> Child {
    let binary =
        std::env::var("ANO_CHRONOTOPE_BIN").expect("ANO_CHRONOTOPE_BIN を指定してください");
    let child = Command::new(binary)
        .args(["serve", "--addr", addr, "--data"])
        .arg(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if reqwest::get(format!("http://{addr}/healthz")).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("chronotope が起動しませんでした");
    child
}
async fn query(endpoint: &str, user: &str, mut body: Value) -> Value {
    body["budget_ms"] = json!(1000);
    let response = reqwest::Client::new()
        .post(format!("{endpoint}/v1/query"))
        .header("x-chronotope-principal", "ano-test")
        .header("x-chronotope-kind", "agent")
        .header("x-chronotope-on-behalf-of", user)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
    response.json::<Value>().await.unwrap()
}
async fn cli(root: &Path, model: &str, args: &[&str], stdin: Option<&str>) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ano"))
        .current_dir(root)
        .env("OPENAI_BASE_URL", model)
        .env("OPENAI_API_KEY", "fixture")
        .args(["--config", "config.toml"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .await
            .unwrap();
    }
    drop(child.stdin.take());
    tokio::time::timeout(Duration::from_secs(40), child.wait_with_output())
        .await
        .unwrap()
        .unwrap()
}
fn successful(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
#[ignore = "ANO_CHRONOTOPE_BIN で実サーバを指定する結合テスト"]
async fn real_history_cli_round_trip_compaction_restart_and_offline_retry() {
    let root = tempfile::tempdir().unwrap();
    let addr = free_addr();
    let endpoint = format!("http://{addr}");
    let mut database = start_database(&root.path().join("db"), &addr).await;
    let original = "  前回の依頼は取り消し🙏\t次の条件で。  ";
    let raw_tool = "省略してはいけないツール結果😀\n".repeat(5000);
    assert!(raw_tool.len() > 128 * 1024);
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let app=Router::new().route("/v1/responses",post({
        let requests=requests.clone(); let raw_tool=raw_tool.clone();
        move |Json(payload):Json<Value>| {
            let requests=requests.clone(); let raw_tool=raw_tool.clone();
            async move {
                if payload["instructions"].as_str().unwrap_or("").starts_with("You compact") {
                    return Json(json!({"id":"summary","status":"completed","output":[message("以前の会話を圧縮した要約です。原文は履歴ツールで確認してください。")]}));
                }
                let mut captured=requests.lock().unwrap();
                captured.push(payload.clone());
                let n=captured.len();
                let output=match n {
                    1=>call("search-echo","tool_search",json!({"query":"echo"})),
                    2=>call("large-output","echo",json!({"value":raw_tool})),
                    3=>{
                        assert_eq!(last_result(&payload)["truncated"],true);
                        message("最初の作業を記録しました。")
                    },
                    4=>call("search-history","tool_search",json!({"query":"history_search history_get history_context"})),
                    5=>call("lookup","history_search",json!({"text":"前回の依頼は取り消し🙏","exact":true,"origins":["human"]})),
                    6=>{
                        let found=last_result(&payload);
                        assert_eq!(found["results"]["events"].as_array().unwrap().len(),1);
                        call("get-original","history_get",json!({"event":found["results"]["events"][0]["event_id"]}))
                    },
                    7=>{
                        let found=last_result(&payload);
                        assert_eq!(found["results"]["content"]["text"],original);
                        call("get-context","history_context",json!({"event":found["results"]["event"]["event_id"]}))
                    },
                    8=>{
                        let found=last_result(&payload);
                        assert!(found["results"]["events"].as_array().unwrap().len()>1);
                        message(original)
                    },
                    _=>message("再送テストの応答です。"),
                };
                Json(json!({"id":format!("r{n}"),"status":"completed","output":[output]}))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::fs::write(root.path().join("config.toml"),format!("[api]\nmax_retries=0\n[history]\nenabled=true\nbase_url='{endpoint}'\ndata_dir='history'\ntimeout_secs=1\n")).unwrap();
    let output = cli(
        root.path(),
        &model,
        &["chat", "--session", "session.json", "--quiet"],
        Some(&format!(
            "{original}\n/compact\n  以前の発言を原文で確認  \n/exit\n"
        )),
    )
    .await;
    successful(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains(original));
    assert_eq!(requests.lock().unwrap().len(), 8);
    let session = ano::Session::inspect(root.path().join("session.json")).unwrap();
    assert_eq!(session.compactions.len(), 1);
    let found=query(&endpoint,"default",json!({"op":"history_search","conversation":session.conversation_id,"origins":["human"],"kinds":["message"],"order":"sequence"})).await;
    let users = found["results"]["events"].as_array().unwrap();
    assert_eq!(users.len(), 2);
    let first_id = users[0]["event_id"].as_str().unwrap().to_string();
    let tool=query(&endpoint,"default",json!({"op":"history_search","call_id":"large-output","kinds":["tool_result"],"conversation":session.conversation_id,"order":"sequence"})).await;
    let mut joined = String::new();
    for event in tool["results"]["events"].as_array().unwrap() {
        let mut offset = 0;
        loop {
            let page=query(&endpoint,"default",json!({"op":"history_get","event":event["event_id"],"offset":offset,"length":16001})).await;
            joined.push_str(page["results"]["content"]["text"].as_str().unwrap());
            match page["results"]["content"]["next_offset"].as_u64() {
                Some(next) => offset = next,
                None => break,
            }
        }
    }
    assert_eq!(
        serde_json::from_str::<Value>(&joined).unwrap(),
        json!(raw_tool)
    );
    // 所有者の指定を変えても、別のユーザーからは原文を取得できない。
    let denied = reqwest::Client::new()
        .post(format!("{endpoint}/v1/query"))
        .header("x-chronotope-principal", "bob")
        .json(&json!({"op":"history_get","owner":"default","event":first_id,"budget_ms":1000}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::NOT_FOUND);

    // DB 停止中でも原文を保存し、別のプロセスから再送する。
    database.kill().await.unwrap();
    let offline = "  停止中に受け付けた原文\n改行も維持。  ";
    let output = cli(root.path(), &model, &["run", "--quiet", offline], None).await;
    successful(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("原文はローカルに保持"));
    let status = cli(root.path(), &model, &["history", "status"], None).await;
    successful(&status);
    assert!(
        serde_json::from_slice::<Value>(&status.stdout).unwrap()["pending_events"]
            .as_u64()
            .unwrap()
            > 0
    );
    database = start_database(&root.path().join("db"), &addr).await;
    let sync = cli(root.path(), &model, &["history", "sync"], None).await;
    successful(&sync);
    assert_eq!(
        serde_json::from_slice::<Value>(&sync.stdout).unwrap()["pending_events"],
        0
    );
    let again = cli(root.path(), &model, &["history", "sync"], None).await;
    successful(&again);
    assert_eq!(
        serde_json::from_slice::<Value>(&again.stdout).unwrap()["sent_events"],
        0
    );
    let search = cli(
        root.path(),
        &model,
        &["history", "search", offline, "--exact", "--human"],
        None,
    )
    .await;
    successful(&search);
    let found: Value = serde_json::from_slice(&search.stdout).unwrap();
    assert_eq!(found["results"]["events"].as_array().unwrap().len(), 1);
    let read = query(
        &endpoint,
        "default",
        json!({"op":"history_get","event":first_id}),
    )
    .await;
    assert_eq!(read["results"]["content"]["text"], original);
    database.kill().await.unwrap();
    server.abort();
}
