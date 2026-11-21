use super::*;
use crate::infrastructure::chronotope::HistorySettings;
use axum::{routing::post, Json, Router};
use std::sync::Mutex;

fn at(text: &str) -> DateTime<chrono::FixedOffset> {
    DateTime::parse_from_rfc3339(text).unwrap()
}

#[test]
fn times_are_read_relative_to_the_publication() {
    let published = at("2026-11-20T09:00:00+09:00");
    let read = |when: &str| read_event_time(when, &published).map(|time| time.label);
    assert_eq!(
        read("11月28日から29日まで").unwrap(),
        "2026-11-28〜2026-11-29"
    );
    assert_eq!(read("12月5日18時").unwrap(), "2026-12-05 18:00");
    assert_eq!(read("12月5日").unwrap(), "2026-12-05");
    assert_eq!(read("来週末").unwrap(), "2026-11-28〜2026-11-29");
    // A month-long event is an event, unlike in the agenda.
    assert_eq!(read("12月").unwrap(), "2026-12-01〜2026-12-31");
    let span = read_event_time("11月28日から29日まで", &published).unwrap();
    assert_eq!(span.end - span.start, 2 * 24 * 60 * 60);
    // A time zone offset followed by kanji once panicked in chronotope-core.
    assert!(read("2026-12-05 15:30-12時30").is_err());
    for when in ["毎週金曜", "そのうち", "2026年12月以降"] {
        assert!(read(when).is_err(), "{when}");
    }
}

#[test]
fn the_publication_is_a_date_or_a_time_and_never_in_the_future() {
    let now = at("2026-11-21T12:00:00+09:00").timestamp() as u64;
    assert_eq!(
        published_at(Some("2026-11-20T09:00:00+09:00"), now)
            .unwrap()
            .timestamp(),
        at("2026-11-20T09:00:00+09:00").timestamp()
    );
    assert!(published_at(Some("2026-11-20"), now).is_ok());
    assert_eq!(published_at(None, now).unwrap().timestamp(), now as i64);
    assert!(published_at(Some("2026-12-01"), now).is_err());
    assert!(published_at(Some("先週"), now).is_err());
}

struct Fake {
    requests: Mutex<Vec<(String, Value)>>,
    /// Replies to `/v1/query`, in order; the last one repeats.
    queries: Mutex<Vec<Value>>,
}

async fn serve(fake: Arc<Fake>) -> (String, tokio::task::JoinHandle<()>) {
    let route = |path: &'static str| {
        let fake = Arc::clone(&fake);
        post(move |Json(body): Json<Value>| {
            let fake = Arc::clone(&fake);
            async move {
                fake.requests
                    .lock()
                    .unwrap()
                    .push((path.to_string(), body.clone()));
                Json(match path {
                    "ingest/text" => json!({
                        "entities": [
                            {"ref": "E1", "resource": format!("res_{}", body["extraction"]["entities"][0]["label"].as_str().unwrap().len())},
                            {"ref": "P1", "resource": "res_place"}
                        ]
                    }),
                    "materialize" => json!({"processed": 1}),
                    _ => {
                        let mut queries = fake.queries.lock().unwrap();
                        if queries.len() > 1 {
                            queries.remove(0)
                        } else {
                            queries[0].clone()
                        }
                    }
                })
            }
        })
    };
    let app = Router::new()
        .route("/v1/ingest/text", route("ingest/text"))
        .route("/v1/materialize", route("materialize"))
        .route("/v1/query", route("query"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, server)
}

fn events(root: &std::path::Path, url: &str) -> Events {
    let chronotope = Chronotope::new(HistorySettings {
        enabled: true,
        base_url: url.into(),
        data_dir: Some(root.join("history")),
        ..Default::default()
    })
    .unwrap();
    Events::new(root.join("events"), Arc::new(chronotope))
}

fn fes() -> Value {
    json!({
        "url": "https://example.jp/news/beer-fes",
        "name": "代々木クラフトビールフェス2026",
        "when": "11月28日から29日まで",
        "place": "代々木公園",
        "passage": "渋谷区の代々木公園で、11月28日から29日まで「代々木クラフトビールフェス2026」が開かれる。",
        "published": "2026-11-20T09:00:00+09:00"
    })
}

#[tokio::test]
async fn an_event_is_saved_once_and_found_by_period_with_its_article() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake {
        requests: Mutex::new(Vec::new()),
        queries: Mutex::new(vec![
            json!({"results": [], "freshness": {"consistent": true}}),
        ]),
    });
    let (url, server) = serve(Arc::clone(&fake)).await;
    let events = events(root.path(), &url);
    let now = at("2026-11-21T12:00:00+09:00").timestamp() as u64;

    let saved = events.save("alice", &fes(), now).await.unwrap();
    assert_eq!(saved["status"], "saved");
    assert_eq!(saved["time"], "2026-11-28〜2026-11-29");
    let again = events.save("alice", &fes(), now).await.unwrap();
    assert_eq!(again["status"], "already_saved");
    let resource = events.saved("alice").unwrap()[0].resource.clone();
    assert!(events.saved("bob").unwrap().is_empty());
    {
        let requests = fake.requests.lock().unwrap();
        let paths: Vec<&str> = requests.iter().map(|(path, _)| path.as_str()).collect();
        // Saved once: the second call is answered from the saved list.
        assert_eq!(paths, ["ingest/text", "materialize"]);
        let ingest = &requests[0].1;
        assert_eq!(
            ingest["document"]["url"],
            "https://example.jp/news/beer-fes"
        );
        assert!(ingest["document"]["text"]
            .as_str()
            .unwrap()
            .contains("11月28日から29日まで"));
        let claims = &ingest["extraction"]["claims"];
        assert_eq!(claims[0]["predicate"], "occurred_at");
        // The time goes to chronotope as written, read in the local calendar.
        assert_eq!(claims[0]["object"]["time"], "11月28日から29日まで");
        // In the offset of when the article was published.
        assert_eq!(claims[0]["object"]["calendar"], "gregorian+09:00");
        assert_eq!(claims[1]["predicate"], "took_place_at");
    }

    // The search keeps the user's own events and adds the article's URL.
    *fake.queries.lock().unwrap() = vec![json!({
        "results": [{"id": "res_someone_else"}, {"id": resource}],
        "freshness": {"consistent": true},
        "truncated": false
    })];
    let found = events
        .search("alice", &json!({"time": "来週末"}), now)
        .await
        .unwrap();
    let list = found["events"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{found}");
    assert_eq!(list[0]["name"], "代々木クラフトビールフェス2026");
    assert_eq!(list[0]["url"], "https://example.jp/news/beer-fes");
    assert_eq!(found["period"], "来週末");
    assert!(found.get("warnings").is_none(), "{found}");
    {
        let requests = fake.requests.lock().unwrap();
        let query = &requests.last().unwrap().1;
        assert_eq!(query["time"]["expression"], "来週末");
        assert_eq!(query["time"]["calendar"], local_calendar());
        assert_eq!(query["types"], json!(["Event"]));
    }

    // Another user has saved nothing, so nothing of alice's shows up.
    let other = events
        .search("bob", &json!({"time": "来週末"}), now)
        .await
        .unwrap();
    assert_eq!(other["events"], json!([]));

    // With no period or text, the saved events that have not ended.
    let upcoming = events.search("alice", &json!({}), now).await.unwrap();
    assert_eq!(upcoming["events"][0]["time"], "2026-11-28〜2026-11-29");
    let later = at("2026-12-01T00:00:00+09:00").timestamp() as u64;
    let upcoming = events.search("alice", &json!({}), later).await.unwrap();
    assert_eq!(upcoming["events"], json!([]));
    server.abort();
}

#[tokio::test]
async fn parallel_saves_of_one_event_reach_chronotope_once() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake {
        requests: Mutex::new(Vec::new()),
        queries: Mutex::new(vec![json!({})]),
    });
    let (url, server) = serve(Arc::clone(&fake)).await;
    let events = events(root.path(), &url);
    let now = at("2026-11-21T12:00:00+09:00").timestamp() as u64;
    let arguments = fes();
    let (first, second) = tokio::join!(
        events.save("alice", &arguments, now),
        events.save("alice", &arguments, now)
    );
    let statuses = [
        first.unwrap()["status"].clone(),
        second.unwrap()["status"].clone(),
    ];
    assert!(statuses.contains(&json!("saved")) && statuses.contains(&json!("already_saved")));
    let ingests = fake
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == "ingest/text")
        .count();
    assert_eq!(ingests, 1);
    server.abort();
}

#[tokio::test]
async fn a_search_on_a_stale_index_materializes_and_asks_again() {
    let root = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake {
        requests: Mutex::new(Vec::new()),
        queries: Mutex::new(vec![json!({})]),
    });
    let (url, server) = serve(Arc::clone(&fake)).await;
    let events = events(root.path(), &url);
    let now = at("2026-11-21T12:00:00+09:00").timestamp() as u64;
    events.save("alice", &fes(), now).await.unwrap();
    let resource = events.saved("alice").unwrap()[0].resource.clone();
    *fake.queries.lock().unwrap() = vec![
        json!({"results": [], "freshness": {"consistent": false}}),
        json!({"results": [{"id": resource}], "freshness": {"consistent": true}}),
    ];
    fake.requests.lock().unwrap().clear();

    let found = events
        .search("alice", &json!({"text": "ビール"}), now)
        .await
        .unwrap();
    assert_eq!(found["events"].as_array().unwrap().len(), 1, "{found}");
    let paths: Vec<String> = fake
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|(path, _)| path.clone())
        .collect();
    assert_eq!(paths, ["query", "materialize", "query"]);

    // Still stale after that: say results may be missing.
    *fake.queries.lock().unwrap() =
        vec![json!({"results": [], "freshness": {"consistent": false}})];
    let found = events
        .search("alice", &json!({"text": "ビール"}), now)
        .await
        .unwrap();
    assert!(found["warnings"][0]
        .as_str()
        .unwrap()
        .contains("not indexed"));
    server.abort();
}

#[tokio::test]
async fn arguments_are_checked_before_anything_is_sent() {
    let root = tempfile::tempdir().unwrap();
    // Nothing listens here: a call that reached the network would fail
    // with a connection error instead of the expected message.
    let events = events(root.path(), "http://127.0.0.1:9");
    let now = at("2026-11-21T12:00:00+09:00").timestamp() as u64;
    let refused = |change: Value, expected: &'static str| {
        let mut arguments = fes();
        for (key, value) in change.as_object().unwrap() {
            arguments[key] = value.clone();
        }
        let events = &events;
        async move {
            let error = events.save("alice", &arguments, now).await.unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
    };
    refused(json!({"url": "file:///etc/passwd"}), "http(s)").await;
    refused(
        json!({"url": "https://user:pw@example.jp/a"}),
        "credentials",
    )
    .await;
    refused(json!({"name": "「フェス」"}), "without quotes").await;
    refused(json!({"passage": " "}), "passage is required").await;
    refused(json!({"when": "毎週金曜"}), "repeats").await;
    refused(json!({"when": "そのうち"}), "cannot read").await;
    refused(json!({"published": "2027-01-01"}), "future").await;
    assert!(events.saved("alice").unwrap().is_empty());
    let error = events
        .search("alice", &json!({"time": "そのうち"}), now)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("cannot read the period"),
        "{error:#}"
    );
}

/// Against a real chronotope: `ANO_CHRONOTOPE_BIN=/path/to/chronotope
/// cargo test --lib real_chronotope -- --ignored`.
#[tokio::test]
#[ignore]
async fn real_chronotope_saves_finds_right_away_and_keeps_the_user_out_of_public_data() {
    let bin = std::env::var("ANO_CHRONOTOPE_BIN").expect("set ANO_CHRONOTOPE_BIN");
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut server = tokio::process::Command::new(bin)
        .args([
            "serve",
            "--in-memory",
            "--addr",
            &format!("127.0.0.1:{port}"),
        ])
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client
            .get(format!("{url}/v1/freshness"))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let root = tempfile::tempdir().unwrap();
    let events = events(root.path(), &url);
    let now = at("2026-11-21T12:00:00+09:00").timestamp() as u64;

    assert_eq!(
        events.save("alice-secret", &fes(), now).await.unwrap()["status"],
        "saved"
    );
    assert_eq!(
        events.save("alice-secret", &fes(), now).await.unwrap()["status"],
        "already_saved"
    );
    // Searched right after saving, by a period that overlaps the event.
    let found = events
        .search("alice-secret", &json!({"time": "2026年11月29日"}), now)
        .await
        .unwrap();
    assert_eq!(found["events"].as_array().unwrap().len(), 1, "{found}");
    assert_eq!(
        found["events"][0]["url"],
        "https://example.jp/news/beer-fes"
    );
    let none = events
        .search("alice-secret", &json!({"time": "2026年12月"}), now)
        .await
        .unwrap();
    assert_eq!(none["events"], json!([]), "{none}");
    let by_text = events
        .search("alice-secret", &json!({"text": "クラフトビール"}), now)
        .await
        .unwrap();
    assert_eq!(by_text["events"].as_array().unwrap().len(), 1, "{by_text}");

    // The public data names ano as the one that read the article, not the user.
    let resource = events.saved("alice-secret").unwrap()[0].resource.clone();
    let claims: Value = client
        .post(format!("{url}/v1/query"))
        .header("x-chronotope-principal", "someone-else")
        .json(&json!({"op": "expand_claims", "budget_ms": 500, "id": resource}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let text = claims.to_string();
    assert!(text.contains("代々木公園"), "{text}");
    assert!(!text.contains("alice-secret"), "{text}");
    server.kill().await.unwrap();
}
