use super::*;
use crate::{
    domain::{compaction::CompactionRecord, plan::TaskPlan},
    Session,
};
use axum::{http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

fn binding(user: &str) -> SessionBinding {
    SessionBinding {
        user_id: user.into(),
        environment: "test".into(),
        workspace: None,
        endpoint: "http://model.invalid/v1".into(),
    }
}

fn history(root: &Path, endpoint: &str) -> Chronotope {
    Chronotope::new(HistorySettings {
        enabled: true,
        data_dir: root.join("history"),
        base_url: endpoint.into(),
        ..Default::default()
    })
    .unwrap()
}

fn events(history: &Chronotope, user: &str) -> Vec<Value> {
    pending_events(&history.owner_dir(user).unwrap())
        .unwrap()
        .iter()
        .map(|p| serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap())
        .collect()
}

#[test]
fn raw_text_and_full_tool_results_survive_compaction_and_reopening() {
    let root = tempfile::tempdir().unwrap();
    let h = history(root.path(), "http://127.0.0.1:9");
    let path = root.path().join("session.json");
    let original = "  前回の依頼は取り消し🙏\r\n\t次の条件で。 \n";
    let source = json!([{"role":"user","content":[{"type":"input_text","text":original}]}]);
    let runtime = json!([{"role":"user","content":"内部の継続指示"}]);
    let large = "原文を保存😀\r\n".repeat(30_000);
    let full = json!({"type":"function_call_output","call_id":"call-1","output":large});
    let id;
    {
        let mut session = Session::open(&path, binding("alice"), false).unwrap();
        id = session.data().conversation_id.clone();
        let mut store = h.wrap(&mut session).unwrap();
        store
            .begin_turn_with_source(&source, &source, &runtime, "human")
            .unwrap();
        store.record_response("r1", &[json!({"type":"function_call","call_id":"call-1","name":"echo","arguments":"{\"value\":1}"})]).unwrap();
        store
            .checkpoint_tool_result_with_raw(
                &json!({"type":"function_call_output","call_id":"call-1","output":"短縮"}),
                &full,
                &TaskPlan::default(),
            )
            .unwrap();
        store.record_runtime_input(&runtime).unwrap();
        store
            .replace_history(
                vec![json!({"role":"user","content":"要約"})],
                CompactionRecord {
                    id: "compact-1".into(),
                    before_bytes: 100,
                    after_bytes: 10,
                    before_items: 3,
                    after_items: 1,
                    archive_file: None,
                },
            )
            .unwrap();
        store.complete().unwrap();
    }
    {
        let mut session = Session::open(&path, binding("alice"), false).unwrap();
        assert_eq!(session.data().conversation_id, id);
        assert_eq!(session.data().history[0]["content"], "要約");
        let mut store = h.wrap(&mut session).unwrap();
        store
            .begin_turn_with_source(&source, &source, &json!([]), "human")
            .unwrap();
        store.complete().unwrap();
    }
    let events = events(&h, "alice");
    let humans: Vec<_> = events.iter().filter(|e| e["origin"] == "human").collect();
    assert_eq!(humans.len(), 2);
    assert!(humans.iter().all(|e| e["content"] == original));
    let tools: Vec<_> = events
        .iter()
        .filter(|e| e["kind"] == "tool_result")
        .collect();
    assert!(tools.len() > 1);
    let joined: String = tools
        .iter()
        .map(|e| e["content"].as_str().unwrap())
        .collect();
    assert_eq!(joined, large);
    assert!(tools
        .iter()
        .all(|e| e["call_id"] == "call-1" && e["status"] == "ok"));
    assert_eq!(
        tools[0]["metadata"]["content_range"]["sha256"],
        hex::encode(Sha256::digest(large.as_bytes()))
    );
    assert!(events
        .iter()
        .any(|e| e["kind"] == "summary" && e["origin"] == "model"));
    assert!(events
        .iter()
        .enumerate()
        .all(|(i, e)| e["sequence"] == i + 1));
}

#[test]
fn dropped_turn_records_unknown_outcomes_as_runtime_and_never_replays() {
    let root = tempfile::tempdir().unwrap();
    let h = history(root.path(), "http://127.0.0.1:9");
    let mut store = h.transcript_store(binding("alice"));
    {
        let mut recorded = h.wrap(store.as_mut()).unwrap();
        recorded
            .begin_turn_with_source(&json!([]), &json!([]), &json!([]), "human")
            .unwrap();
        recorded.record_response("r", &[json!({"type":"function_call","name":"side_effect","call_id":"pending","arguments":"{}"})]).unwrap();
    }
    assert_eq!(store.data().status, crate::SessionStatus::Failed);
    assert!(store.data().pending_calls().is_empty());
    let events = events(&h, "alice");
    assert!(events.iter().any(|e| e["call_id"] == "pending"
        && e["kind"] == "tool_result"
        && e["status"] == "unknown"
        && e["origin"] == "runtime"));
    assert!(!events.iter().any(|e| e["origin"] == "human"));
}

#[tokio::test]
async fn lost_ack_retries_identical_events_and_validates_every_ack() {
    let root = tempfile::tempdir().unwrap();
    let fail = Arc::new(AtomicBool::new(true));
    let writes = Arc::new(Mutex::new(Vec::<Value>::new()));
    let app = Router::new().route("/v1/write", post({
        let fail=fail.clone(); let writes=writes.clone();
        move |Json(body):Json<Value>| {
            let fail=fail.clone(); let writes=writes.clone();
            async move {
                writes.lock().unwrap().push(body.clone());
                if fail.load(Ordering::SeqCst) { return StatusCode::SERVICE_UNAVAILABLE.into_response(); }
                Json(json!({"owner":"alice","events":body["events"].as_array().unwrap().iter().map(|e|json!({"event_id":e["event_id"],"sequence":e["sequence"],"acquisition":"acq-test"})).collect::<Vec<_>>()})).into_response()
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h = history(
        root.path(),
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut store = h.transcript_store(binding("alice"));
    {
        let mut store = h.wrap(store.as_mut()).unwrap();
        let input = json!([{"role":"user","content":"  再送でも原文を保持\n"}]);
        store
            .begin_turn_with_source(&input, &input, &json!([]), "human")
            .unwrap();
        store.complete().unwrap();
    }
    assert!(h.sync("alice").await.is_err());
    assert_eq!(h.status("alice").unwrap()["pending_events"], 2);
    assert_eq!(h.status("bob").unwrap()["pending_events"], 0);
    fail.store(false, Ordering::SeqCst);
    assert_eq!(h.sync("alice").await.unwrap()["sent_events"], 2);
    assert_eq!(h.status("alice").unwrap()["pending_events"], 0);
    assert_eq!(h.sync("alice").await.unwrap()["sent_events"], 0);
    let writes = writes.lock().unwrap();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0], writes[1]);
    server.abort();
}

#[test]
fn query_arguments_cannot_change_owner_operation_or_unbound_output_size() {
    for args in [
        json!({"owner":"bob"}),
        json!({"op":"record_event"}),
        json!({"budget_ms":999999}),
        json!({"event":"e","length":1000000}),
        json!({"event":"e","offset":-1}),
    ] {
        assert!(tools::validate_query("history_get", args).is_err());
    }
    assert!(tools::validate_query("record_event", json!({})).is_err());
    assert_eq!(
        tools::validate_query("history_get", json!({"event":"e"})).unwrap()["length"],
        4096
    );
}

#[tokio::test]
async fn rejected_event_is_isolated_and_the_sent_position_survives_pruning() {
    let root = tempfile::tempdir().unwrap();
    let writes = Arc::new(Mutex::new(Vec::<Value>::new()));
    // 本物と同様に、1 件でも不正なイベントを含むバッチ全体を拒否する。
    let app = Router::new().route("/v1/write", post({
        let writes = writes.clone();
        move |Json(body): Json<Value>| {
            let writes = writes.clone();
            async move {
                let events = body["events"].as_array().unwrap().clone();
                writes.lock().unwrap().push(body);
                if events.iter().any(|e| e["content"] == "bad") {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                Json(json!({"owner":"alice","events":events.iter().map(|e| json!({"event_id":e["event_id"],"sequence":e["sequence"],"acquisition":"acq"})).collect::<Vec<_>>()})).into_response()
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let h = history(
        root.path(),
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let turn = |store: &mut dyn ConversationStore, texts: &[&str]| {
        let mut store = h.wrap(store).unwrap();
        let input = Value::Array(
            texts
                .iter()
                .map(|t| json!({"role":"user","content":t}))
                .collect(),
        );
        store
            .begin_turn_with_source(&input, &input, &json!([]), "human")
            .unwrap();
        store.complete().unwrap();
    };
    let mut broken = h.transcript_store(binding("alice"));
    turn(broken.as_mut(), &["ok", "bad", "after"]);
    let mut healthy = h.transcript_store(binding("alice"));
    turn(healthy.as_mut(), &["fine"]);

    let error = format!("{:#}", h.sync("alice").await.unwrap_err());
    assert!(error.contains("記録順 2 "), "{error}");
    assert!(error.contains("HTTP 400"), "{error}");
    // 拒否されたイベントより前と、別の会話は送信済みになる。
    let pending = events(&h, "alice");
    assert_eq!(pending.len(), 3);
    assert!(pending
        .iter()
        .all(|e| e["conversation"] == broken.data().conversation_id));
    assert_eq!(pending[0]["content"], "bad");

    // 送信済みの原文を消しても、同じ記録順を再利用しない。
    let dir = h
        .owner_dir("alice")
        .unwrap()
        .join(&healthy.data().conversation_id);
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "json") && path.file_name().unwrap().len() == 25 {
            std::fs::remove_file(path).unwrap();
        }
    }
    turn(healthy.as_mut(), &["again"]);
    let resumed = events(&h, "alice")
        .into_iter()
        .filter(|e| e["conversation"] == healthy.data().conversation_id)
        .map(|e| e["sequence"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(resumed, [3, 4]);
    server.abort();
}

#[test]
fn reasoning_is_not_recorded_even_through_compaction() {
    let root = tempfile::tempdir().unwrap();
    let h = history(root.path(), "http://127.0.0.1:9");
    let path = root.path().join("session.json");
    let reasoning = json!({"type":"reasoning","id":"rs_1","summary":[],"content":[{"type":"reasoning_text","text":"内部の思考"}],"encrypted_content":"opaque"});
    let mut session = Session::open(&path, binding("alice"), false).unwrap();
    let mut store = h.wrap(&mut session).unwrap();
    let input = json!([{"role":"user","content":"質問"}]);
    store
        .begin_turn_with_source(&input, &input, &json!([]), "human")
        .unwrap();
    store
        .record_response(
            "r1",
            &[
                reasoning.clone(),
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"回答"}]}),
            ],
        )
        .unwrap();
    store.complete().unwrap();
    store
        .replace_history(
            vec![
                reasoning,
                json!({"type":"compaction","encrypted_content":"opaque"}),
                json!({"role":"user","content":"要約"}),
            ],
            CompactionRecord {
                id: "compact-1".into(),
                before_bytes: 100,
                after_bytes: 10,
                before_items: 3,
                after_items: 1,
                archive_file: None,
            },
        )
        .unwrap();
    drop(store);

    let events = events(&h, "alice");
    assert!(events.iter().any(|e| e["content"] == "回答"));
    for event in &events {
        let text = event.to_string();
        assert!(
            !text.contains("内部の思考") && !text.contains("opaque"),
            "{text}"
        );
    }
    assert!(events
        .iter()
        .any(|e| e["kind"] == "summary" && e["content"].as_str().unwrap().contains("要約")));
    // 思考過程を除いた履歴はモデル用の履歴とは別で、実行用の履歴はそのまま残す。
    assert_eq!(session.data().history.len(), 3);
}

#[test]
fn switching_the_model_endpoint_keeps_the_raw_history_of_the_conversation() {
    let root = tempfile::tempdir().unwrap();
    let h = history(root.path(), "http://127.0.0.1:9");
    let mut session =
        Session::open(root.path().join("session.json"), binding("alice"), false).unwrap();
    drop(h.wrap(&mut session).unwrap());
    let choice = crate::domain::session::ModelChoice {
        provider: "local".into(),
        model: "qwen".into(),
        reasoning_effort: None,
    };
    h.wrap(&mut session)
        .unwrap()
        .switch_model(&choice, "http://192.168.1.10:1234/v1")
        .unwrap();
    assert_eq!(
        session.data().binding.endpoint,
        "http://192.168.1.10:1234/v1"
    );
    // The same raw history continues on the new endpoint.
    drop(h.wrap(&mut session).unwrap());
    let recorded = events(&h, "alice");
    assert!(recorded.iter().any(|event| event["content"]
        .as_str()
        .is_some_and(|text| text.contains("qwen"))));

    // Another environment is still refused for the same conversation.
    let mut copied = session.data().clone();
    copied.binding.environment = "other".into();
    let other = SessionBinding {
        environment: "other".into(),
        ..session.data().binding.clone()
    };
    drop(session);
    let path = root.path().join("other.json");
    std::fs::write(&path, serde_json::to_vec(&copied).unwrap()).unwrap();
    let mut other = Session::open(&path, other, false).unwrap();
    let error = h.wrap(&mut other).err().unwrap().to_string();
    assert!(error.contains("別のユーザー・環境"), "{error}");
}

#[test]
fn a_conversation_in_use_by_another_run_is_still_refused_after_the_retry() {
    let root = tempfile::tempdir().unwrap();
    let h = history(root.path(), "http://127.0.0.1:9");
    let mut session =
        Session::open(root.path().join("session.json"), binding("alice"), false).unwrap();
    let copied = serde_json::to_vec(session.data()).unwrap();
    let held = h.wrap(&mut session).unwrap();

    // 同じ会話を別のセッションファイルから開く、実際に同時に動いている別の実行。
    let path = root.path().join("other.json");
    std::fs::write(&path, copied).unwrap();
    let mut other = Session::open(&path, binding("alice"), false).unwrap();
    let error = format!("{:#}", h.wrap(&mut other).err().unwrap());
    assert!(error.contains("別の実行が使用中"), "{error}");

    drop(held);
    drop(h.wrap(&mut other).unwrap());
}

#[test]
fn a_lock_released_during_the_retry_is_taken_over() {
    let root = tempfile::tempdir().unwrap();
    let h = history(root.path(), "http://127.0.0.1:9");
    let mut session =
        Session::open(root.path().join("session.json"), binding("alice"), false).unwrap();
    drop(h.wrap(&mut session).unwrap());

    // 起動中の子プロセスが、解放済みのロックの記述子を exec まで持ち続けている状態の代わり。
    let lock = lock_file(
        &h.owner_dir("alice")
            .unwrap()
            .join(&session.data().conversation_id)
            .join("journal.lock"),
    )
    .unwrap();
    lock.try_lock().unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(lock);
    });
    drop(h.wrap(&mut session).unwrap());
    release.join().unwrap();
}
