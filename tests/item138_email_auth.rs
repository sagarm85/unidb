// Item 138 — Email transport + templates + password-reset/magic-link auth
// flows.
//
// No real SMTP, no real network: every test uses `EmailConfig::
// with_log_transport` (the dev-inbox log transport, item 138) pointed at a
// per-test temp file, and recovers the emailed token by reading that file
// back — exactly the "capture the outbound email from the transport" the
// task spec asks for.
//
// Test matrix (mirrors the backlog doc's acceptance list):
//   (a) recover_then_verify_sets_new_password_and_login_works
//   (b) recover_unknown_email_returns_200_and_sends_nothing
//   (c) recover_known_vs_unknown_response_is_indistinguishable (enumeration)
//   (d) verify_token_is_single_use
//   (e) verify_rejects_garbage_or_unknown_token
//   (f) magiclink_then_verify_issues_a_session
//   (g) magiclink_unknown_email_returns_200_and_sends_nothing
//   (h) email_bodies_are_never_logged_at_info_by_dev_inbox_json_shape (a
//       structural sanity check that the captured record is well-formed and
//       carries exactly one token, not a spot check of `tracing` output —
//       see `src/server/email.rs`'s own unit tests for the `Debug`-redaction
//       coverage)
//
// TTL expiry itself is unit-tested in `src/authz/mod.rs`
// (`recovery_token_rejects_garbage_and_expired`) by directly manipulating a
// token record's `expires_at`, rather than sleeping across a real ~1h/15m
// window here — same precedent as item 127/128's MFA-challenge/OAuth-state
// TTLs, neither of which has a real-time-sleep integration test either.

#![cfg(feature = "server")]

use reqwest::StatusCode;
use serde_json::{json, Value};
use unidb::server::email::EmailConfig;

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

/// Every JSON line currently in the dev-inbox file, oldest first. Empty
/// (not an error) if the file doesn't exist yet — mirrors "nothing sent".
fn captured_emails(path: &std::path::Path) -> Vec<Value> {
    let contents = std::fs::read_to_string(path).unwrap_or_default();
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("dev-inbox line must be valid JSON"))
        .collect()
}

/// Pull the `token=...` query value out of a captured email's `text_body`
/// (every default template embeds the link there) — the raw single-use
/// token this test then redeems at `POST /auth/verify`/`/auth/magiclink/verify`.
fn extract_token(email: &Value) -> String {
    let text = email["text_body"]
        .as_str()
        .expect("text_body must be a string");
    let idx = text
        .find("token=")
        .expect("email body must contain a token= link");
    text[idx + "token=".len()..]
        .split(|c: char| c.is_whitespace())
        .next()
        .expect("token value must not be empty")
        .to_string()
}

/// Returns `(server, _inbox_dir, inbox_file_path)` — the caller must keep
/// `_inbox_dir` alive (bind it, even if unused) for the lifetime of the
/// test, since dropping it deletes the directory the dev-inbox file lives
/// in.
async fn spawn_with_inbox() -> (TestServer, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let inbox = dir.path().join("inbox.jsonl");
    let email = EmailConfig::with_log_transport(&inbox, "unidb@localhost", "http://localhost:8080");
    let server = TestServer::spawn_with_dev_login_signup_and_email(email).await;
    (server, dir, inbox)
}

async fn signup(client: &reqwest::Client, server: &TestServer, username: &str, password: &str) {
    let resp = client
        .post(server.url("/auth/signup"))
        .json(&json!({"username": username, "password": password}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn login_status(
    client: &reqwest::Client,
    server: &TestServer,
    username: &str,
    password: &str,
) -> StatusCode {
    client
        .post(server.url("/auth/login"))
        .json(&json!({"username": username, "password": password}))
        .send()
        .await
        .unwrap()
        .status()
}

// ── (a) recover -> verify -> new password works, old password doesn't ─────

#[tokio::test]
async fn recover_then_verify_sets_new_password_and_login_works() {
    let (server, _inbox_dir, inbox) = spawn_with_inbox().await;
    let client = reqwest::Client::new();
    signup(&client, &server, "alice@example.com", "old-password-123").await;

    let resp = client
        .post(server.url("/auth/recover"))
        .json(&json!({"email": "alice@example.com"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    let emails = captured_emails(&inbox);
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0]["to"], "alice@example.com");
    let token = extract_token(&emails[0]);

    let verify_resp = client
        .post(server.url("/auth/verify"))
        .json(&json!({"token": token, "new_password": "new-password-456"}))
        .send()
        .await
        .unwrap();
    assert_eq!(verify_resp.status(), StatusCode::OK);

    // Old password no longer works, new one does.
    assert_eq!(
        login_status(&client, &server, "alice@example.com", "old-password-123").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login_status(&client, &server, "alice@example.com", "new-password-456").await,
        StatusCode::OK
    );
}

// ── (b) unknown email -> 200, nothing sent ─────────────────────────────────

#[tokio::test]
async fn recover_unknown_email_returns_200_and_sends_nothing() {
    let (server, _inbox_dir, inbox) = spawn_with_inbox().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/auth/recover"))
        .json(&json!({"email": "nobody-registered@example.com"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    assert!(captured_emails(&inbox).is_empty());
}

// ── (c) known vs unknown email: response-indistinguishable ─────────────────

#[tokio::test]
async fn recover_known_vs_unknown_response_is_indistinguishable() {
    let (server, _inbox_dir, inbox) = spawn_with_inbox().await;
    let client = reqwest::Client::new();
    signup(&client, &server, "bob@example.com", "some-password").await;

    let known_resp = client
        .post(server.url("/auth/recover"))
        .json(&json!({"email": "bob@example.com"}))
        .send()
        .await
        .unwrap();
    let known_status = known_resp.status();
    let known_body: Value = known_resp.json().await.unwrap();

    let unknown_resp = client
        .post(server.url("/auth/recover"))
        .json(&json!({"email": "definitely-not-a-user@example.com"}))
        .send()
        .await
        .unwrap();
    let unknown_status = unknown_resp.status();
    let unknown_body: Value = unknown_resp.json().await.unwrap();

    assert_eq!(known_status, unknown_status);
    assert_eq!(known_body, unknown_body);
    // But only the known email actually got an email minted.
    assert_eq!(captured_emails(&inbox).len(), 1);
}

// ── (d) recovery token is single-use ────────────────────────────────────────

#[tokio::test]
async fn verify_token_is_single_use() {
    let (server, _inbox_dir, inbox) = spawn_with_inbox().await;
    let client = reqwest::Client::new();
    signup(&client, &server, "carol@example.com", "first-password").await;

    client
        .post(server.url("/auth/recover"))
        .json(&json!({"email": "carol@example.com"}))
        .send()
        .await
        .unwrap();
    let token = extract_token(&captured_emails(&inbox)[0]);

    let first = client
        .post(server.url("/auth/verify"))
        .json(&json!({"token": token, "new_password": "second-password"}))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    // Replaying the exact same token must fail — single-use.
    let second = client
        .post(server.url("/auth/verify"))
        .json(&json!({"token": token, "new_password": "third-password"}))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::UNAUTHORIZED);
    let body: Value = second.json().await.unwrap();
    assert_eq!(body["code"], "RECOVERY_TOKEN_INVALID");

    // The password from the first (successful) redemption is still active.
    assert_eq!(
        login_status(&client, &server, "carol@example.com", "second-password").await,
        StatusCode::OK
    );
}

// ── (e) garbage/unknown token -> uniform 401 ────────────────────────────────

#[tokio::test]
async fn verify_rejects_garbage_or_unknown_token() {
    let (server, _inbox_dir, _inbox) = spawn_with_inbox().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/auth/verify"))
        .json(&json!({"token": "not-a-real-token", "new_password": "whatever"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "RECOVERY_TOKEN_INVALID");
}

// ── (f) magic link: request -> capture -> verify -> real session ───────────

#[tokio::test]
async fn magiclink_then_verify_issues_a_session() {
    let (server, _inbox_dir, inbox) = spawn_with_inbox().await;
    let client = reqwest::Client::new();
    signup(&client, &server, "dave@example.com", "irrelevant-password").await;

    let resp = client
        .post(server.url("/auth/magiclink"))
        .json(&json!({"email": "dave@example.com"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let emails = captured_emails(&inbox);
    assert_eq!(emails.len(), 1);
    let token = extract_token(&emails[0]);

    let verify_resp = client
        .post(server.url("/auth/magiclink/verify"))
        .json(&json!({"token": token}))
        .send()
        .await
        .unwrap();
    assert_eq!(verify_resp.status(), StatusCode::OK);
    let body: Value = verify_resp.json().await.unwrap();
    assert!(body.get("access_token").and_then(Value::as_str).is_some());
    assert!(body.get("refresh_token").and_then(Value::as_str).is_some());
    assert_eq!(body["expires_in"], 3600);

    // The redeemed token must not work a second time.
    let replay = client
        .post(server.url("/auth/magiclink/verify"))
        .json(&json!({"token": token}))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    let replay_body: Value = replay.json().await.unwrap();
    assert_eq!(replay_body["code"], "MAGICLINK_TOKEN_INVALID");
}

// ── (g) magic link: unknown email -> 200, nothing sent ──────────────────────

#[tokio::test]
async fn magiclink_unknown_email_returns_200_and_sends_nothing() {
    let (server, _inbox_dir, inbox) = spawn_with_inbox().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/auth/magiclink"))
        .json(&json!({"email": "ghost@example.com"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert!(captured_emails(&inbox).is_empty());
}

// ── (h) captured email is well-formed and carries exactly one token ────────

#[tokio::test]
async fn captured_recovery_email_is_well_formed_json_with_one_token() {
    let (server, _inbox_dir, inbox) = spawn_with_inbox().await;
    let client = reqwest::Client::new();
    signup(&client, &server, "erin@example.com", "pw-erin").await;

    client
        .post(server.url("/auth/recover"))
        .json(&json!({"email": "erin@example.com"}))
        .send()
        .await
        .unwrap();

    let emails = captured_emails(&inbox);
    assert_eq!(emails.len(), 1);
    let email = &emails[0];
    assert_eq!(email["from"], "unidb@localhost");
    assert!(email["subject"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("password"));
    let text = email["text_body"].as_str().unwrap();
    assert_eq!(text.matches("token=").count(), 1);
    let html = email["html_body"].as_str().unwrap();
    assert!(html.contains("token="));
}
