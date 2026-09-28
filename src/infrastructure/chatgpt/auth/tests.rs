use super::*;
use axum::{extract::Form, routing::post, Json, Router};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

fn token(account: &str) -> String {
    format!(
        "header.{}.signature",
        URL_SAFE_NO_PAD.encode(
            json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": account}
            })
            .to_string()
        )
    )
}

fn credentials(expires_at: u64) -> Credentials {
    Credentials {
        access_token: "old-access".into(),
        refresh_token: "old-refresh".into(),
        account_id: "account".into(),
        expires_at,
    }
}

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}

#[tokio::test]
async fn device_login_polls_exchanges_pkce_and_saves_private_credentials() {
    let polls = Arc::new(Mutex::new(0));
    let polled = polls.clone();
    let exchanges = Arc::new(Mutex::new(Vec::new()));
    let exchanged = exchanges.clone();
    let app = Router::new()
        .route("/api/accounts/deviceauth/usercode", post(|Json(body): Json<Value>| async move {
            assert_eq!(body["client_id"], CLIENT_ID);
            Json(json!({"usercode":"test-code", "device_auth_id":"device", "interval":"1"}))
        }))
        .route("/api/accounts/deviceauth/token", post(move |Json(body): Json<Value>| {
            let polled = polled.clone();
            async move {
                assert_eq!(body, json!({"user_code":"test-code", "device_auth_id":"device"}));
                let mut polls = polled.lock().unwrap();
                *polls += 1;
                if *polls == 1 {
                    (StatusCode::FORBIDDEN, Json(json!({})))
                } else {
                    (StatusCode::OK, Json(json!({"authorization_code":"grant", "code_verifier":"verifier"})))
                }
            }
        }))
        .route("/oauth/token", post(move |Form(fields): Form<HashMap<String, String>>| {
            let exchanged = exchanged.clone();
            async move {
                exchanged.lock().unwrap().push(fields);
                Json(json!({"access_token":token("account"), "refresh_token":"refresh", "expires_in":3600}))
            }
        }));
    let (issuer, server) = serve(app).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nested/auth.json");
    let auth = ChatGptAuth::for_test(&path, &issuer);
    auth.login(|url, code| {
        assert_eq!(url, format!("{issuer}/codex/device"));
        assert_eq!(code, "test-code");
    })
    .await
    .unwrap();
    server.abort();
    assert_eq!(*polls.lock().unwrap(), 2);
    let fields = exchanges.lock().unwrap()[0].clone();
    assert_eq!(fields["grant_type"], "authorization_code");
    assert_eq!(fields["code"], "grant");
    assert_eq!(fields["code_verifier"], "verifier");
    assert_eq!(fields["client_id"], CLIENT_ID);
    assert_eq!(
        fields["redirect_uri"],
        format!("{issuer}/deviceauth/callback")
    );
    let saved = auth.load().unwrap().unwrap();
    assert_eq!(saved.account_id, "account");
    assert_eq!(saved.refresh_token, "refresh");
    let status = auth.status().unwrap();
    assert!(status.logged_in);
    assert!(!status.needs_refresh);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    assert!(auth.logout().await.unwrap());
    assert!(!auth.status().unwrap().logged_in);
    assert!(!auth.logout().await.unwrap());
}

#[tokio::test]
async fn concurrent_clients_refresh_once_and_preserve_rotated_tokens() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let captured = calls.clone();
    let (issuer, server) = serve(Router::new().route("/oauth/token", post(move |Form(fields): Form<HashMap<String, String>>| {
        let captured = captured.clone();
        async move {
            captured.lock().unwrap().push(fields);
            tokio::time::sleep(Duration::from_millis(80)).await;
            Json(json!({"access_token":token("account"), "refresh_token":"rotated", "expires_in":3600}))
        }
    }))).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth.json");
    let first = ChatGptAuth::for_test(&path, &issuer);
    let second = ChatGptAuth::for_test(&path, &issuer);
    first.save(&credentials(0)).unwrap();
    let (a, b) = tokio::join!(
        first.credentials(None),
        second.credentials(Some("old-access"))
    );
    assert_eq!(a.unwrap().access_token, b.unwrap().access_token);
    server.abort();
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["refresh_token"], "old-refresh");
    assert_eq!(calls[0]["grant_type"], "refresh_token");
    assert_eq!(first.load().unwrap().unwrap().refresh_token, "rotated");
}

#[tokio::test]
async fn failed_refresh_preserves_credentials_and_hides_response_body() {
    let (issuer, server) = serve(Router::new().route(
        "/oauth/token",
        post(|| async {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"invalid_grant", "secret":"never-print-me"})),
            )
        }),
    ))
    .await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth.json");
    let auth = ChatGptAuth::for_test(&path, &issuer);
    auth.save(&credentials(0)).unwrap();
    let before = std::fs::read(&path).unwrap();
    let error = auth.credentials(None).await.err().unwrap().to_string();
    server.abort();
    assert!(error.contains("ano auth login"));
    assert!(!error.contains("never-print-me"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn token_refresh_allows_missing_refresh_token_but_login_requires_account() {
    let old = credentials(0);
    let updated = credentials_from_tokens(
        &json!({"access_token":"opaque", "expires_in":3600}),
        Some(&old),
    )
    .unwrap();
    assert_eq!(updated.refresh_token, old.refresh_token);
    assert_eq!(updated.account_id, old.account_id);
    assert!(credentials_from_tokens(
        &json!({"access_token":"opaque", "refresh_token":"refresh"}),
        None
    )
    .is_err());
}

#[test]
fn malformed_credentials_do_not_leak_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth.json");
    std::fs::write(
        &path,
        r#"{"access_token": "secret", "expires_at": "private-data"}"#,
    )
    .unwrap();
    let auth = ChatGptAuth::new(Some(&path)).unwrap();
    let error = auth.status().unwrap_err().to_string();
    assert!(!error.contains("secret"));
    assert!(!error.contains("private-data"));
}
