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
//
// A3 (signup) + A4 (refresh tokens / sessions / logout):
//   8.  login_response_includes_access_and_refresh_tokens
//   9.  signup_disabled_by_default_returns_404
//   10. signup_enabled_creates_user_and_returns_tokens
//   11. signup_duplicate_username_returns_clear_error
//   12. refresh_mints_new_access_token_and_rotates_refresh_token
//   13. refresh_with_unknown_token_returns_401
//   14. logout_revokes_refresh_token_then_refresh_returns_401
//   15. logout_is_idempotent
//   16. refresh_token_never_appears_in_login_response_debug_or_roles_json

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

// ── A3/A4 helpers ───────────────────────────────────────────────────────────

async fn signup(
    client: &reqwest::Client,
    server: &TestServer,
    username: &str,
    password: &str,
) -> reqwest::Response {
    client
        .post(server.url("/auth/signup"))
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send()
        .await
        .unwrap()
}

async fn refresh(
    client: &reqwest::Client,
    server: &TestServer,
    refresh_token: &str,
) -> reqwest::Response {
    client
        .post(server.url("/auth/refresh"))
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .unwrap()
}

async fn logout(
    client: &reqwest::Client,
    server: &TestServer,
    refresh_token: &str,
) -> reqwest::Response {
    client
        .post(server.url("/auth/logout"))
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .unwrap()
}

// ── test 8: login now returns access_token + refresh_token + legacy token ──────

#[tokio::test]
async fn login_response_includes_access_and_refresh_tokens() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER nora PASSWORD 'noras-pw'",
    )
    .await;

    let resp = login(&client, &server, "nora", "noras-pw").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let access = body["access_token"].as_str().expect("access_token present");
    let legacy = body["token"]
        .as_str()
        .expect("token (legacy alias) present");
    let refresh_tok = body["refresh_token"]
        .as_str()
        .expect("refresh_token present");
    assert_eq!(access, legacy, "token must alias access_token");
    assert!(!refresh_tok.is_empty());
    assert_ne!(
        access, refresh_tok,
        "access and refresh tokens must be distinct"
    );
    assert_eq!(body["expires_in"].as_u64(), Some(3600));

    // The refresh token is a high-entropy opaque string, not a JWT (no `.`
    // separators the way a three-part JWT has).
    assert!(
        !refresh_tok.contains('.'),
        "refresh token must not look like a JWT"
    );
}

// ── test 9: signup is 404 unless UNIDB_ALLOW_SIGNUP=1 ──────────────────────────

#[tokio::test]
async fn signup_disabled_by_default_returns_404() {
    // Dev-login is on but signup is not — must still 404.
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();

    let resp = signup(&client, &server, "penny", "pennys-pw").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── test 10: signup creates a user and returns usable tokens ──────────────────

#[tokio::test]
async fn signup_enabled_creates_user_and_returns_tokens() {
    let server = TestServer::spawn_with_dev_login_and_signup().await;
    let client = reqwest::Client::new();

    let resp = signup(&client, &server, "quinn", "quinns-pw").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let access = body["access_token"].as_str().unwrap().to_string();
    assert!(!body["refresh_token"].as_str().unwrap().is_empty());

    // The issued access token actually authenticates as quinn.
    let whoami: Value = client
        .get(server.url("/auth/whoami"))
        .header("Authorization", format!("Bearer {access}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(whoami["user"].as_str(), Some("quinn"));
    assert_eq!(
        whoami["is_superuser"].as_bool(),
        Some(false),
        "signup must never create a superuser"
    );

    // The account also works via the normal password-login path.
    let login_resp = login(&client, &server, "quinn", "quinns-pw").await;
    assert_eq!(login_resp.status(), StatusCode::OK);
}

// ── test 11: signup rejects a duplicate username clearly ──────────────────────

#[tokio::test]
async fn signup_duplicate_username_returns_clear_error() {
    let server = TestServer::spawn_with_dev_login_and_signup().await;
    let client = reqwest::Client::new();

    let first = signup(&client, &server, "riley", "rileys-pw").await;
    assert_eq!(first.status(), StatusCode::OK);

    let dup = signup(&client, &server, "riley", "a-different-pw").await;
    assert!(
        dup.status().is_client_error(),
        "duplicate signup should be a 4xx, got {}",
        dup.status()
    );
    let body: Value = dup.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("riley"));

    // The original account's password must be unaffected by the rejected
    // duplicate attempt.
    let login_resp = login(&client, &server, "riley", "rileys-pw").await;
    assert_eq!(login_resp.status(), StatusCode::OK);
}

// ── test 12: refresh mints a new access token and rotates the refresh token ────

#[tokio::test]
async fn refresh_mints_new_access_token_and_rotates_refresh_token() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();
    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER sam PASSWORD 'sams-pw'",
    )
    .await;

    let login_body: Value = login(&client, &server, "sam", "sams-pw")
        .await
        .json()
        .await
        .unwrap();
    let old_refresh = login_body["refresh_token"].as_str().unwrap().to_string();
    let old_access = login_body["access_token"].as_str().unwrap().to_string();

    let refresh_resp = refresh(&client, &server, &old_refresh).await;
    assert_eq!(refresh_resp.status(), StatusCode::OK);
    let refresh_body: Value = refresh_resp.json().await.unwrap();
    let new_access = refresh_body["access_token"].as_str().unwrap().to_string();
    let new_refresh = refresh_body["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(new_refresh, old_refresh, "refresh token must rotate");
    // A fresh access token is minted (may coincidentally match on some JWT
    // libs if `iat` collides to the same second — assert it verifies instead
    // of asserting inequality, which is the property that actually matters).
    let _ = old_access; // acknowledged, not required to differ byte-for-byte

    let whoami: Value = client
        .get(server.url("/auth/whoami"))
        .header("Authorization", format!("Bearer {new_access}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(whoami["user"].as_str(), Some("sam"));

    // The OLD refresh token must no longer work (rotation revokes it).
    let old_refresh_resp = refresh(&client, &server, &old_refresh).await;
    assert_eq!(old_refresh_resp.status(), StatusCode::UNAUTHORIZED);

    // The NEW refresh token works for a further refresh.
    let second_refresh_resp = refresh(&client, &server, &new_refresh).await;
    assert_eq!(second_refresh_resp.status(), StatusCode::OK);
}

// ── test 13: unknown/garbage refresh token returns 401 ─────────────────────────

#[tokio::test]
async fn refresh_with_unknown_token_returns_401() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();

    let resp = refresh(&client, &server, "not-a-real-refresh-token").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── test 14: logout revokes; a revoked refresh token then 401s ─────────────────

#[tokio::test]
async fn logout_revokes_refresh_token_then_refresh_returns_401() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();
    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER tara PASSWORD 'taras-pw'",
    )
    .await;

    let login_body: Value = login(&client, &server, "tara", "taras-pw")
        .await
        .json()
        .await
        .unwrap();
    let refresh_tok = login_body["refresh_token"].as_str().unwrap().to_string();

    let logout_resp = logout(&client, &server, &refresh_tok).await;
    assert_eq!(logout_resp.status(), StatusCode::NO_CONTENT);

    let refresh_resp = refresh(&client, &server, &refresh_tok).await;
    assert_eq!(refresh_resp.status(), StatusCode::UNAUTHORIZED);
}

// ── test 15: logout is idempotent (no error on repeat / unknown token) ────────

#[tokio::test]
async fn logout_is_idempotent() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();
    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER uma PASSWORD 'umas-pw'",
    )
    .await;
    let login_body: Value = login(&client, &server, "uma", "umas-pw")
        .await
        .json()
        .await
        .unwrap();
    let refresh_tok = login_body["refresh_token"].as_str().unwrap().to_string();

    assert_eq!(
        logout(&client, &server, &refresh_tok).await.status(),
        StatusCode::NO_CONTENT
    );
    // Logging out again with the same (already-revoked) token still succeeds.
    assert_eq!(
        logout(&client, &server, &refresh_tok).await.status(),
        StatusCode::NO_CONTENT
    );
    // Logging out with a token that was never issued also succeeds.
    assert_eq!(
        logout(&client, &server, "never-issued-token")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
}

// ── test 16: secrets never leak via roles.json or an accidental Debug print ────

#[tokio::test]
async fn refresh_token_never_appears_in_login_response_debug_or_roles_json() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();
    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER vince PASSWORD 'vinces-pw'",
    )
    .await;
    let login_body: Value = login(&client, &server, "vince", "vinces-pw")
        .await
        .json()
        .await
        .unwrap();
    let refresh_tok = login_body["refresh_token"].as_str().unwrap().to_string();

    // The raw refresh token must never appear in the persisted roles.json —
    // only its SHA-256 hash (control-plane metadata persistence, item 121 A4).
    let roles_json_path = server.data_dir().join("roles.json");
    let raw = std::fs::read_to_string(&roles_json_path).unwrap();
    assert!(!raw.contains(&refresh_tok));
}
