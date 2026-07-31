// Item 121 (Workstream A) — A1 credential store + A2 real password login.
//
// Test matrix:
//   1. create_user_with_password_then_login_succeeds — signup-less
//      create-user-with-password → correct-password login → 200 + usable token.
//   2. wrong_password_returns_401
//   3. unknown_user_returns_401_same_shape_as_wrong_password — no
//      user-enumeration oracle: identical status + body shape.
//   4. user_without_credential_cannot_login — CREATE USER with no PASSWORD
//      clause cannot log in via password (not silently passwordless).
//   5. password_never_appears_in_whoami — the stored hash never leaks via
//      GET /auth/whoami.
//   6. open_mode_unchanged_by_auth_core — open_mode still reflects
//      has_users(), unaffected by credential-store changes.
//   7. create_user_password_clause_with_special_chars — spaces + an escaped
//      quote in the PASSWORD literal round-trip correctly end-to-end.

#![cfg(feature = "server")]

use reqwest::StatusCode;
use serde_json::Value;

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

async fn create_user(client: &reqwest::Client, server: &TestServer, admin_tok: &str, sql: &str) {
    client
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {admin_tok}"))
        .json(&serde_json::json!({ "sql": sql }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}

async fn login(
    client: &reqwest::Client,
    server: &TestServer,
    username: &str,
    password: &str,
) -> reqwest::Response {
    client
        .post(server.url("/auth/login"))
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send()
        .await
        .unwrap()
}

// ── test 1: create-user-with-password → correct-password login → 200 ──────────

#[tokio::test]
async fn create_user_with_password_then_login_succeeds() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER alice PASSWORD 'sekrit-pw-123'",
    )
    .await;

    let resp = login(&client, &server, "alice", "sekrit-pw-123").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let token = body["token"].as_str().expect("token field must be present");
    assert!(!token.is_empty());
    assert_eq!(body["expires_in"].as_u64(), Some(3600));

    // The issued token must actually authenticate as alice.
    let whoami: Value = client
        .get(server.url("/auth/whoami"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(whoami["user"].as_str(), Some("alice"));
}

// ── test 2: wrong password → 401 ───────────────────────────────────────────────

#[tokio::test]
async fn wrong_password_returns_401() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER bob PASSWORD 'the-real-password'",
    )
    .await;

    let resp = login(&client, &server, "bob", "not-the-real-password").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── test 3: unknown user → 401, identical shape to wrong-password ─────────────

#[tokio::test]
async fn unknown_user_returns_401_same_shape_as_wrong_password() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER carol PASSWORD 'carols-password'",
    )
    .await;

    let wrong_pw_resp = login(&client, &server, "carol", "definitely-wrong").await;
    let wrong_pw_status = wrong_pw_resp.status();
    let wrong_pw_body: Value = wrong_pw_resp.json().await.unwrap();

    let unknown_user_resp = login(&client, &server, "no-such-user", "definitely-wrong").await;
    let unknown_user_status = unknown_user_resp.status();
    let unknown_user_body: Value = unknown_user_resp.json().await.unwrap();

    assert_eq!(wrong_pw_status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown_user_status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        wrong_pw_status, unknown_user_status,
        "wrong-password and unknown-user must return the identical status"
    );
    // Same body shape (both keys, no distinguishing content) — no
    // user-enumeration oracle via the response body either.
    assert_eq!(
        wrong_pw_body.as_object().map(|o| {
            let mut ks: Vec<_> = o.keys().cloned().collect();
            ks.sort();
            ks
        }),
        unknown_user_body.as_object().map(|o| {
            let mut ks: Vec<_> = o.keys().cloned().collect();
            ks.sort();
            ks
        }),
        "response body shape must match between the two failure cases"
    );
    assert_eq!(
        wrong_pw_body["error"], unknown_user_body["error"],
        "error message must be identical text, not user-specific"
    );
}

// ── test 4: a user with no stored credential cannot log in via password ───────

#[tokio::test]
async fn user_without_credential_cannot_login() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    // No PASSWORD clause — dave has no stored credential at all.
    create_user(&client, &server, &admin_tok, "CREATE USER dave").await;

    let resp = login(&client, &server, "dave", "").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp2 = login(&client, &server, "dave", "some-guess").await;
    assert_eq!(resp2.status(), StatusCode::UNAUTHORIZED);
}

// ── test 5: the stored hash never leaks via GET /auth/whoami ──────────────────

#[tokio::test]
async fn password_never_appears_in_whoami() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER erin PASSWORD 'super-duper-secret'",
    )
    .await;

    let token = {
        let body: Value = login(&client, &server, "erin", "super-duper-secret")
            .await
            .json()
            .await
            .unwrap();
        body["token"].as_str().unwrap().to_string()
    };

    let whoami_resp = client
        .get(server.url("/auth/whoami"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let whoami_text = whoami_resp.text().await.unwrap();
    assert!(!whoami_text.contains("super-duper-secret"));
    assert!(!whoami_text.contains("argon2"));
}

// ── test 6: open_mode is unaffected by the credential-store changes ───────────

#[tokio::test]
async fn open_mode_unchanged_by_auth_core() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();

    // Before any user: open mode is true.
    let meta_before: Value = client
        .get(server.url("/auth/meta"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(meta_before["open_mode"].as_bool(), Some(true));

    let admin_tok = server_common::valid_token();
    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER frank PASSWORD 'franks-pw'",
    )
    .await;

    // After a user exists (even one with a password credential): open mode
    // is false, exactly as before item 121.
    let meta_after: Value = client
        .get(server.url("/auth/meta"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(meta_after["open_mode"].as_bool(), Some(false));
}

// ── test 7: PASSWORD clause with spaces + an escaped quote round-trips ────────

#[tokio::test]
async fn create_user_password_clause_with_special_chars() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    // SQL string-literal escaping: '' inside a '...' literal is one literal
    // quote. The actual password is: a b'c d
    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER gina PASSWORD 'a b''c d'",
    )
    .await;

    let resp = login(&client, &server, "gina", "a b'c d").await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The wrong (unescaped) form must not work.
    let resp_wrong = login(&client, &server, "gina", "a b''c d").await;
    assert_eq!(resp_wrong.status(), StatusCode::UNAUTHORIZED);
}
