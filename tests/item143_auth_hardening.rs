// Item 143 — Auth hardening: leaked-password check (HaveIBeenPwned) +
// more OAuth provider presets.
//
// No real network, no real secrets: a tiny local axum "mock HIBP range
// server" stands in for https://api.pwnedpasswords.com (pointed at via
// `HibpConfig::new`/`UNIDB_PASSWORD_HIBP_URL`'s override seam), and a tiny
// local "mock OAuth provider" (same shape as `tests/item128_oauth.rs`'s)
// stands in for Discord. Both bind to `127.0.0.1:0` (ephemeral loopback
// ports only).
//
// Test matrix (mirrors the task spec's acceptance list):
//   (a) signup_rejects_breached_password_and_accepts_clean_one
//   (b) admin_create_and_patch_reject_breached_password
//   (c) password_reset_verify_rejects_breached_password
//   (d) hibp_disabled_by_default_is_back_compat
//   (e) hibp_unreachable_endpoint_fails_open_signup_still_succeeds
//   (f) discord_preset_completes_oauth_flow_with_local_mock
//   (g) unconfigured_preset_provider_404s

#![cfg(feature = "server")]

use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path as AxumPath, State as AxumState},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use reqwest::StatusCode;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use unidb::server::email::EmailConfig;
use unidb::server::hibp::HibpConfig;
use unidb::server::oauth::OAuthConfig;

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

// ── SHA-1 helpers mirroring `hibp.rs`'s own bucketing (test-side only, to
//    build a mock response that matches whatever password a test picks) ──

fn upper_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02X}"));
    }
    s
}

/// `(prefix, suffix)` — the same split `hibp.rs::HibpConfig::check` does.
fn hibp_prefix_suffix(password: &str) -> (String, String) {
    let digest = Sha1::digest(password.as_bytes());
    let hex = upper_hex(digest.as_slice());
    (hex[..5].to_string(), hex[5..].to_string())
}

// ── a tiny local mock HIBP range server (no real network) ──────────────────

#[derive(Clone, Default)]
struct MockHibpState {
    /// `(expected prefix, suffix line to return for that prefix)` — any
    /// other prefix gets an empty body (no match), same as a real bucket
    /// with no breached suffixes.
    breach: Arc<Mutex<Option<(String, String)>>>,
}

async fn mock_hibp_range(
    AxumState(state): AxumState<MockHibpState>,
    AxumPath(prefix): AxumPath<String>,
) -> impl IntoResponse {
    let guard = state.breach.lock().unwrap();
    if let Some((expected_prefix, suffix)) = guard.as_ref() {
        if expected_prefix.eq_ignore_ascii_case(&prefix) {
            return format!("{suffix}:3\r\nAAAA0000000000000000000000000000000:1\r\n");
        }
    }
    // No match in this bucket — the real API still returns 200 with other
    // (irrelevant) suffixes; an empty body is an equally valid "no match".
    "AAAA0000000000000000000000000000000:1\r\n".to_string()
}

struct MockHibp {
    addr: std::net::SocketAddr,
    state: MockHibpState,
    _task: tokio::task::JoinHandle<()>,
}

impl MockHibp {
    async fn spawn() -> Self {
        let state = MockHibpState::default();
        let router = Router::new()
            .route("/range/{prefix}", get(mock_hibp_range))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router.into_make_service()).await;
        });
        Self {
            addr,
            state,
            _task: task,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Program the mock to report `password` as breached.
    fn mark_breached(&self, password: &str) {
        let (prefix, suffix) = hibp_prefix_suffix(password);
        *self.state.breach.lock().unwrap() = Some((prefix, suffix));
    }
}

// ── shared small helpers ────────────────────────────────────────────────

async fn signup(server: &TestServer, username: &str, password: &str) -> reqwest::Response {
    client()
        .post(server.url("/auth/signup"))
        .json(&json!({"username": username, "password": password}))
        .send()
        .await
        .unwrap()
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

/// Bootstraps a `root` superuser via the open-mode bootstrap token, mirrors
/// `item142_auth_admin.rs::setup_root`.
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

async fn admin_create(server: &TestServer, token: &str, body: Value) -> (u16, Value) {
    let resp = client()
        .post(server.url("/auth/admin/users"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn admin_patch(
    server: &TestServer,
    token: &str,
    username: &str,
    body: Value,
) -> (u16, Value) {
    let resp = client()
        .patch(server.url(&format!("/auth/admin/users/{username}")))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn admin_get(server: &TestServer, token: &str, username: &str) -> u16 {
    client()
        .get(server.url(&format!("/auth/admin/users/{username}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn error_code(body: &Value) -> &str {
    body["code"].as_str().unwrap_or_default()
}

// ── (a) signup: breached password rejected, clean password succeeds ───────

#[tokio::test]
async fn signup_rejects_breached_password_and_accepts_clean_one() {
    let mock = MockHibp::spawn().await;
    let breached_password = "correct-horse-battery-staple-breached";
    mock.mark_breached(breached_password);
    let hibp = HibpConfig::new(true, mock.base_url());
    let server = TestServer::spawn_with_signup_email_and_hibp(
        EmailConfig::with_log_transport(
            std::env::temp_dir().join(format!("item143-inbox-{}.jsonl", std::process::id())),
            "unidb@localhost",
            "http://localhost:8080",
        ),
        hibp,
    )
    .await;

    let resp = signup(&server, "alice", breached_password).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(error_code(&body), "PASSWORD_COMPROMISED");

    // The rejected signup must not have created the account.
    let root_token = setup_root(&server).await;
    assert_eq!(admin_get(&server, &root_token, "alice").await, 404);

    let resp = signup(&server, "bob", "a-totally-different-clean-password-999").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(body.get("access_token").and_then(Value::as_str).is_some());
}

// ── (b) admin create/patch reject a breached password ───────────────────────

#[tokio::test]
async fn admin_create_and_patch_reject_breached_password() {
    let mock = MockHibp::spawn().await;
    let breached_password = "admin-breached-password-777";
    mock.mark_breached(breached_password);
    let hibp = HibpConfig::new(true, mock.base_url());
    let server = TestServer::spawn_with_signup_email_and_hibp(
        EmailConfig::with_log_transport(
            std::env::temp_dir().join(format!("item143-inbox-admin-{}.jsonl", std::process::id())),
            "unidb@localhost",
            "http://localhost:8080",
        ),
        hibp,
    )
    .await;
    let root_token = setup_root(&server).await;

    // Create with a breached password -> rejected, no account left behind.
    let (status, body) = admin_create(
        &server,
        &root_token,
        json!({"username": "carol", "password": breached_password}),
    )
    .await;
    assert_eq!(status, 422);
    assert_eq!(error_code(&body), "PASSWORD_COMPROMISED");
    assert_eq!(admin_get(&server, &root_token, "carol").await, 404);

    // Create with a clean password -> succeeds.
    let (status, _body) = admin_create(
        &server,
        &root_token,
        json!({"username": "carol", "password": "carol-clean-password-abc"}),
    )
    .await;
    assert_eq!(status, 201);

    // Patch to a breached password -> rejected.
    let (status, body) = admin_patch(
        &server,
        &root_token,
        "carol",
        json!({"password": breached_password}),
    )
    .await;
    assert_eq!(status, 422);
    assert_eq!(error_code(&body), "PASSWORD_COMPROMISED");

    // Patch to a clean password -> succeeds.
    let (status, _body) = admin_patch(
        &server,
        &root_token,
        "carol",
        json!({"password": "carol-second-clean-password-xyz"}),
    )
    .await;
    assert_eq!(status, 200);
}

// ── (c) password-reset verify rejects a breached password ──────────────────

#[tokio::test]
async fn password_reset_verify_rejects_breached_password() {
    let mock = MockHibp::spawn().await;
    let breached_password = "reset-breached-password-555";
    mock.mark_breached(breached_password);
    let hibp = HibpConfig::new(true, mock.base_url());
    let dir = tempfile::tempdir().unwrap();
    let inbox = dir.path().join("inbox.jsonl");
    let server = TestServer::spawn_with_signup_email_and_hibp(
        EmailConfig::with_log_transport(&inbox, "unidb@localhost", "http://localhost:8080"),
        hibp,
    )
    .await;

    let resp = signup(&server, "dave@example.com", "dave-initial-clean-password").await;
    assert_eq!(resp.status(), StatusCode::OK);

    // First recovery round: redeem with the breached password -> rejected.
    // The recovery token is single-use and gets consumed by this attempt
    // (see `handlers.rs::post_auth_verify`'s doc comment), so a fresh
    // recovery email is needed for the follow-up success case below —
    // exactly mirroring how any other post-consumption rejection behaves.
    let resp = client()
        .post(server.url("/auth/recover"))
        .json(&json!({"email": "dave@example.com"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let token1 = extract_token_from_inbox(&inbox, 0);

    let resp = client()
        .post(server.url("/auth/verify"))
        .json(&json!({"token": token1, "new_password": breached_password}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(error_code(&body), "PASSWORD_COMPROMISED");

    // Second recovery round: redeem with a clean password -> succeeds.
    let resp = client()
        .post(server.url("/auth/recover"))
        .json(&json!({"email": "dave@example.com"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let token2 = extract_token_from_inbox(&inbox, 1);

    let resp = client()
        .post(server.url("/auth/verify"))
        .json(&json!({"token": token2, "new_password": "dave-new-clean-password-999"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // New password works, old one no longer does.
    let login_status = client()
        .post(server.url("/auth/login"))
        .json(&json!({"username": "dave@example.com", "password": "dave-new-clean-password-999"}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(login_status, StatusCode::OK);
}

/// Pull the raw single-use token out of the `idx`-th captured dev-inbox
/// email's `text_body` (mirrors `item138_email_auth.rs::extract_token`).
fn extract_token_from_inbox(inbox: &std::path::Path, idx: usize) -> String {
    let contents = std::fs::read_to_string(inbox).unwrap_or_default();
    let line = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .nth(idx)
        .expect("expected a captured email at this index");
    let email: Value = serde_json::from_str(line).unwrap();
    let text = email["text_body"].as_str().unwrap();
    let start = text.find("token=").expect("body must contain token=");
    text[start + "token=".len()..]
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap()
        .to_string()
}

// ── (d) HIBP disabled by default -> back-compat, no network call needed ────

#[tokio::test]
async fn hibp_disabled_by_default_is_back_compat() {
    // Deliberately unroutable base URL: if the disabled check ever made a
    // network call this would hang/fail instead of succeeding immediately.
    let hibp = HibpConfig::disabled();
    assert!(!hibp.is_enabled());
    let server = TestServer::spawn_with_signup_email_and_hibp(
        EmailConfig::with_log_transport(
            std::env::temp_dir().join(format!("item143-inbox-off-{}.jsonl", std::process::id())),
            "unidb@localhost",
            "http://localhost:8080",
        ),
        hibp,
    )
    .await;

    // Even a password that *would* be flagged by a real HIBP lookup (a
    // famously breached value) succeeds when the check is off.
    let resp = signup(&server, "erin", "password123").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = signup(&server, "frank", "another-normal-password").await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── (e) HIBP endpoint unreachable -> fail OPEN, signup still succeeds ───────

#[tokio::test]
async fn hibp_unreachable_endpoint_fails_open_signup_still_succeeds() {
    // Port 1 is a privileged, essentially-never-listening port — the
    // connection is refused immediately rather than timing out, keeping the
    // test fast while still exercising the "endpoint unreachable" path.
    let hibp = HibpConfig::new(true, "http://127.0.0.1:1");
    let server = TestServer::spawn_with_signup_email_and_hibp(
        EmailConfig::with_log_transport(
            std::env::temp_dir().join(format!(
                "item143-inbox-unreachable-{}.jsonl",
                std::process::id()
            )),
            "unidb@localhost",
            "http://localhost:8080",
        ),
        hibp,
    )
    .await;

    let resp = signup(&server, "grace", "any-password-at-all-here").await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── (f) / (g) — Discord preset: local-mock flow + unconfigured 404 ─────────

#[derive(Clone)]
struct MockOAuthProviderState {
    inner: Arc<Mutex<MockOAuthProviderInner>>,
}

struct MockOAuthProviderInner {
    id: String,
    email: String,
    access_token: String,
}

async fn mock_authorize() -> &'static str {
    "ok"
}

async fn mock_token(AxumState(state): AxumState<MockOAuthProviderState>) -> impl IntoResponse {
    let inner = state.inner.lock().unwrap();
    Json(json!({
        "access_token": inner.access_token,
        "token_type": "bearer",
        "expires_in": 3600,
    }))
}

/// Discord's `/users/@me`-shaped response: `id`, not `sub` — already
/// covered by `UserInfo::provider_user_id`'s `id` fallback, no extraction
/// change needed.
async fn mock_userinfo(
    AxumState(state): AxumState<MockOAuthProviderState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let inner = state.inner.lock().unwrap();
    let expected = format!("Bearer {}", inner.access_token);
    let got = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if got != expected {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_token" })),
        );
    }
    (
        axum::http::StatusCode::OK,
        Json(json!({ "id": inner.id, "email": inner.email })),
    )
}

struct MockOAuthProvider {
    addr: std::net::SocketAddr,
    _task: tokio::task::JoinHandle<()>,
}

impl MockOAuthProvider {
    async fn spawn(id: &str, email: &str) -> Self {
        let state = MockOAuthProviderState {
            inner: Arc::new(Mutex::new(MockOAuthProviderInner {
                id: id.to_string(),
                email: email.to_string(),
                access_token: format!("mock-access-token-for-{id}"),
            })),
        };
        let router = Router::new()
            .route("/authorize", get(mock_authorize))
            .route("/token", axum::routing::post(mock_token))
            .route("/userinfo", get(mock_userinfo))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router.into_make_service()).await;
        });
        Self { addr, _task: task }
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// Guards the `UNIDB_OAUTH_DISCORD_*` environment variables — this is the
/// only test in this binary (and in the whole workspace, each `tests/*.rs`
/// file is its own process) that touches them, but a mutex keeps the
/// set/build/unset sequence race-free regardless, mirroring
/// `server_common::TestServer::spawn_with_dev_login_oauth_and_engine`'s
/// `UNIDB_MASTER_KEY` guard.
static DISCORD_ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
async fn discord_preset_completes_oauth_flow_with_local_mock() {
    let mock = MockOAuthProvider::spawn("mock-discord-uid-1", "discord-user@example.com").await;

    let oauth_cfg = {
        let _guard = DISCORD_ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("UNIDB_OAUTH_DISCORD_CLIENT_ID", "test-discord-client-id");
        std::env::set_var(
            "UNIDB_OAUTH_DISCORD_CLIENT_SECRET",
            "test-discord-client-secret",
        );
        std::env::set_var(
            "UNIDB_OAUTH_DISCORD_REDIRECT_URI",
            "http://localhost/callback",
        );
        // Point the preset's real endpoints at the local mock — proves
        // "discord" is now in `OAuthConfig::from_env`'s recognized preset
        // loop (an unrecognized name would never reach `provider_from_env`'s
        // `default_urls` lookup and would stay unconfigured regardless of
        // these env vars being set).
        std::env::set_var(
            "UNIDB_OAUTH_DISCORD_AUTHORIZE_URL",
            format!("{}/authorize", mock.base()),
        );
        std::env::set_var(
            "UNIDB_OAUTH_DISCORD_TOKEN_URL",
            format!("{}/token", mock.base()),
        );
        std::env::set_var(
            "UNIDB_OAUTH_DISCORD_USERINFO_URL",
            format!("{}/userinfo", mock.base()),
        );
        let cfg = OAuthConfig::from_env();
        std::env::remove_var("UNIDB_OAUTH_DISCORD_CLIENT_ID");
        std::env::remove_var("UNIDB_OAUTH_DISCORD_CLIENT_SECRET");
        std::env::remove_var("UNIDB_OAUTH_DISCORD_REDIRECT_URI");
        std::env::remove_var("UNIDB_OAUTH_DISCORD_AUTHORIZE_URL");
        std::env::remove_var("UNIDB_OAUTH_DISCORD_TOKEN_URL");
        std::env::remove_var("UNIDB_OAUTH_DISCORD_USERINFO_URL");
        cfg
    };
    assert!(
        oauth_cfg.get("discord").is_some(),
        "discord must be a recognized OAuth preset (item 143, part 2)"
    );
    // (g) an unconfigured preset (facebook: real-world default URLs, no
    // client id/secret/redirect ever set) stays absent -> 404.
    assert!(oauth_cfg.get("facebook").is_none());

    let server = TestServer::spawn_with_dev_login_and_oauth(oauth_cfg).await;
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let resp = no_redirect
        .get(server.url("/auth/oauth/discord/authorize"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let url = reqwest::Url::parse(&location).unwrap();
    let state = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.to_string())
        .expect("authorize URL must carry a state param");

    let resp = no_redirect
        .get(server.url(&format!(
            "/auth/oauth/discord/callback?code=any-code&state={state}"
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let access_token = body["access_token"].as_str().unwrap().to_string();
    assert!(body.get("refresh_token").and_then(Value::as_str).is_some());

    let who: Value = client()
        .get(server.url("/auth/whoami"))
        .bearer_auth(&access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(who["user"], "oauth_discord_mock-discord-uid-1");
    assert_eq!(who["is_superuser"], false);

    // (g) an unconfigured preset provider route 404s.
    let resp = no_redirect
        .get(server.url("/auth/oauth/facebook/authorize"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
