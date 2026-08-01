#![cfg(feature = "server")]
//! Item 145 — Dev-inbox read route (`GET`/`DELETE /auth/dev-inbox`).
//!
//! Studio-flagged gap on item 138: `LogTransport` writes every "sent" email
//! to a dev-inbox JSONL file, but there was no route to read it back. This
//! adds a double-gated read/clear surface: 404 unless the active email
//! transport is the dev-inbox log transport (never reachable with real SMTP
//! configured), then 403 for a non-superuser.
//!
//! No real SMTP, no real network: the dev-transport tests use
//! `EmailConfig::with_log_transport` (exactly like `tests/item138_
//! email_auth.rs`); the SMTP-gate test uses `EmailConfig::with_smtp_transport`
//! purely for its *shape* (never actually sends).
//!
//! Test matrix (mirrors the backlog doc's acceptance list):
//!   (a) recover_then_dev_inbox_returns_entry_and_token_is_redeemable
//!   (b) delete_dev_inbox_empties_it
//!   (c) non_superuser_gets_403
//!   (d) entries_are_newest_first_and_limit_is_respected
//!   (e) smtp_transport_configured_get_and_delete_return_404

use reqwest::StatusCode;
use serde_json::{json, Value};
use unidb::server::email::{EmailConfig, SmtpConfig, SmtpTlsMode};

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// Returns `(server, _inbox_dir, inbox_file_path)` — the caller must keep
/// `_inbox_dir` alive (bind it, even if unused) for the lifetime of the
/// test, since dropping it deletes the directory the dev-inbox file lives
/// in. Mirrors `tests/item138_email_auth.rs::spawn_with_inbox`.
async fn spawn_with_inbox() -> (TestServer, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let inbox = dir.path().join("inbox.jsonl");
    let email = EmailConfig::with_log_transport(&inbox, "unidb@localhost", "http://localhost:8080");
    let server = TestServer::spawn_with_dev_login_signup_and_email(email).await;
    (server, dir, inbox)
}

async fn sql(server: &TestServer, token: &str, sql_text: &str) -> (u16, Value) {
    let resp = client()
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "sql": sql_text }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

/// Bootstraps a `root` superuser via the open-mode bootstrap token — must
/// run before any other user is created (open mode ends the moment a user
/// exists). Mirrors `item142_auth_admin.rs::setup_root`.
async fn setup_root(server: &TestServer) -> String {
    let bootstrap = server_common::valid_token();
    assert_eq!(
        sql(server, &bootstrap, "CREATE USER root SUPERUSER")
            .await
            .0,
        200
    );
    server_common::token_for("root")
}

async fn signup(server: &TestServer, username: &str, password: &str) {
    let resp = client()
        .post(server.url("/auth/signup"))
        .json(&json!({"username": username, "password": password}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn recover(server: &TestServer, email: &str) {
    let resp = client()
        .post(server.url("/auth/recover"))
        .json(&json!({"email": email}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn magiclink(server: &TestServer, email: &str) {
    let resp = client()
        .post(server.url("/auth/magiclink"))
        .json(&json!({"email": email}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn get_dev_inbox(server: &TestServer, token: &str, limit: Option<usize>) -> (u16, Value) {
    let mut url = server.url("/auth/dev-inbox");
    if let Some(l) = limit {
        url = format!("{url}?limit={l}");
    }
    let resp = client()
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

async fn delete_dev_inbox(server: &TestServer, token: &str) -> u16 {
    client()
        .delete(server.url("/auth/dev-inbox"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// Pull the `token=...` query value out of a captured email's `text_body`
/// (every default template embeds the link there) — mirrors `tests/item138_
/// email_auth.rs::extract_token`.
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

// ── (a) recover -> GET /auth/dev-inbox (superuser) -> redeem the token ─────

#[tokio::test]
async fn recover_then_dev_inbox_returns_entry_and_token_is_redeemable() {
    let (server, _inbox_dir, _inbox) = spawn_with_inbox().await;
    let root = setup_root(&server).await;
    signup(&server, "alice@example.com", "old-password-123").await;
    recover(&server, "alice@example.com").await;

    let (status, body) = get_dev_inbox(&server, &root, None).await;
    assert_eq!(status, 200);
    let entries = body.as_array().expect("response must be a JSON array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["to"], "alice@example.com");
    assert!(entries[0]["subject"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("password"));
    assert!(entries[0]["ts"].as_u64().unwrap() > 0);

    let token = extract_token(&entries[0]);
    let verify_resp = client()
        .post(server.url("/auth/verify"))
        .json(&json!({"token": token, "new_password": "new-password-456"}))
        .send()
        .await
        .unwrap();
    assert_eq!(verify_resp.status(), StatusCode::OK);

    // Redeeming actually rotated the password.
    let login_resp = client()
        .post(server.url("/auth/login"))
        .json(&json!({"username": "alice@example.com", "password": "new-password-456"}))
        .send()
        .await
        .unwrap();
    assert_eq!(login_resp.status(), StatusCode::OK);
}

// ── (b) DELETE /auth/dev-inbox empties it ──────────────────────────────────

#[tokio::test]
async fn delete_dev_inbox_empties_it() {
    let (server, _inbox_dir, _inbox) = spawn_with_inbox().await;
    let root = setup_root(&server).await;
    signup(&server, "bob@example.com", "some-password").await;
    recover(&server, "bob@example.com").await;

    let (status, body) = get_dev_inbox(&server, &root, None).await;
    assert_eq!(status, 200);
    assert_eq!(body.as_array().unwrap().len(), 1);

    assert_eq!(delete_dev_inbox(&server, &root).await, 204);

    let (status, body) = get_dev_inbox(&server, &root, None).await;
    assert_eq!(status, 200);
    assert!(body.as_array().unwrap().is_empty());
}

// ── (c) non-superuser -> 403 on both routes ────────────────────────────────

#[tokio::test]
async fn non_superuser_gets_403() {
    let (server, _inbox_dir, _inbox) = spawn_with_inbox().await;
    setup_root(&server).await;
    signup(&server, "plain@example.com", "some-password").await;
    let plain = server_common::token_for("plain@example.com");

    assert_eq!(get_dev_inbox(&server, &plain, None).await.0, 403);
    assert_eq!(delete_dev_inbox(&server, &plain).await, 403);
}

// ── (d) newest-first ordering + limit ──────────────────────────────────────

#[tokio::test]
async fn entries_are_newest_first_and_limit_is_respected() {
    let (server, _inbox_dir, _inbox) = spawn_with_inbox().await;
    let root = setup_root(&server).await;
    signup(&server, "carol@example.com", "some-password").await;
    recover(&server, "carol@example.com").await;
    magiclink(&server, "carol@example.com").await;

    let (status, body) = get_dev_inbox(&server, &root, None).await;
    assert_eq!(status, 200);
    let entries = body.as_array().unwrap();
    assert_eq!(entries.len(), 2);
    // The magic-link email was sent second, so it must come first
    // (newest-first).
    assert!(entries[0]["subject"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("sign"));
    assert!(entries[1]["subject"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("password"));

    let (status, limited) = get_dev_inbox(&server, &root, Some(1)).await;
    assert_eq!(status, 200);
    let limited = limited.as_array().unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0], entries[0]);
}

// ── (e) SMTP configured -> 404 for both routes, superuser or not ──────────

#[tokio::test]
async fn smtp_transport_configured_get_and_delete_return_404() {
    let smtp_email = EmailConfig::with_smtp_transport(
        SmtpConfig::new(
            "smtp.example.test",
            587,
            "smtp-user",
            "smtp-password",
            SmtpTlsMode::Starttls,
        ),
        "unidb@example.test",
        "http://localhost:8080",
    );
    let server = TestServer::spawn_with_dev_login_signup_and_email(smtp_email).await;
    let root = setup_root(&server).await;

    assert_eq!(get_dev_inbox(&server, &root, None).await.0, 404);
    assert_eq!(delete_dev_inbox(&server, &root).await, 404);

    // Also 404 (not merely 403) for a non-superuser — the route must be
    // indistinguishable from non-existent regardless of caller identity.
    signup(&server, "dana@example.com", "some-password").await;
    let plain = server_common::token_for("dana@example.com");
    assert_eq!(get_dev_inbox(&server, &plain, None).await.0, 404);
    assert_eq!(delete_dev_inbox(&server, &plain).await, 404);
}
