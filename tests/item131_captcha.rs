// Item 131 (Workstream I2) — CAPTCHA / bot-protection verification gating
// POST /auth/login and POST /auth/signup, complementing the I1 rate limiter.
//
// No real provider secrets and no real network: a tiny local axum "mock
// siteverify server" (Turnstile-shaped: POST secret+response(+remoteip)
// form-encoded, JSON {success: bool} back) stands in for
// https://challenges.cloudflare.com/turnstile/v0/siteverify, and
// `CaptchaConfig::new` points a test server's captcha config at it via the
// same override seam `UNIDB_CAPTCHA_VERIFY_URL` gives a real deployment.
// Both the mock verifier and the unidb-under-test server bind to
// `127.0.0.1:0` (ephemeral loopback ports only).
//
// Test matrix (mirrors the task spec's acceptance list):
//   (a) signup_and_login_with_valid_token_succeed_when_captcha_enabled
//   (b) signup_rejects_missing_token_and_creates_no_user
//   (c) login_rejects_bad_or_missing_token_with_no_session
//   (d) captcha_disabled_by_default_is_back_compat
//   (e) secret_resolves_from_vault_when_stored_else_falls_back_to_env

#![cfg(feature = "server")]

use std::sync::{Arc, Mutex};

use axum::{extract::State as AxumState, routing::post, Form, Json, Router};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use unidb::server::captcha::{CaptchaConfig, CAPTCHA_SECRET_NAME};

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

/// A 32-byte AES-256 test master key (64 hex chars) — test-only, mirrors
/// `item129_vault.rs`'s constant of the same shape (never a real deployment
/// key).
const TEST_MASTER_KEY_HEX: &str =
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// The one token value the mock verifier treats as "human" — anything else
/// (including an empty/missing field) is "success: false".
const GOOD_TOKEN: &str = "good-captcha-token";
const BAD_TOKEN: &str = "bad-captcha-token";

#[derive(Deserialize)]
struct SiteverifyForm {
    secret: String,
    response: String,
    #[serde(default)]
    #[allow(dead_code)]
    remoteip: Option<String>,
}

#[derive(Clone, Default)]
struct CapturedSecret(Arc<Mutex<Option<String>>>);

/// `POST /siteverify` — returns `{"success": true}` only for [`GOOD_TOKEN`];
/// records whatever `secret` field the request actually carried, so tests
/// can assert vault-vs-env resolution (mirrors `item129_vault.rs`'s
/// `mock_token` capturing pattern).
async fn mock_siteverify(
    AxumState(captured): AxumState<CapturedSecret>,
    Form(form): Form<SiteverifyForm>,
) -> Json<Value> {
    *captured.0.lock().unwrap() = Some(form.secret);
    Json(json!({ "success": form.response == GOOD_TOKEN }))
}

struct MockVerifier {
    addr: std::net::SocketAddr,
    captured: CapturedSecret,
    _task: tokio::task::JoinHandle<()>,
}

impl MockVerifier {
    async fn spawn() -> Self {
        let captured = CapturedSecret::default();
        let router = Router::new()
            .route("/siteverify", post(mock_siteverify))
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router.into_make_service()).await;
        });
        Self {
            addr,
            captured,
            _task: task,
        }
    }

    fn verify_url(&self) -> String {
        format!("http://{}/siteverify", self.addr)
    }

    fn last_captured_secret(&self) -> Option<String> {
        self.captured.0.lock().unwrap().clone()
    }
}

// ── (a) valid token -> signup and login both proceed ───────────────────────

#[tokio::test]
async fn signup_and_login_with_valid_token_succeed_when_captcha_enabled() {
    let mock = MockVerifier::spawn().await;
    let captcha = CaptchaConfig::new("env-secret", mock.verify_url(), ["signup", "login"]);
    let (server, _engine) = TestServer::spawn_with_signup_and_captcha(captcha, None).await;
    let client = reqwest::Client::new();

    let signup_resp = client
        .post(server.url("/auth/signup"))
        .json(&json!({
            "username": "alice",
            "password": "correct horse battery staple",
            "captcha_token": GOOD_TOKEN,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(signup_resp.status(), StatusCode::OK);
    let signup_body: Value = signup_resp.json().await.unwrap();
    assert!(signup_body
        .get("access_token")
        .and_then(Value::as_str)
        .is_some());
    assert!(signup_body
        .get("refresh_token")
        .and_then(Value::as_str)
        .is_some());

    let login_resp = client
        .post(server.url("/auth/login"))
        .json(&json!({
            "username": "alice",
            "password": "correct horse battery staple",
            "captcha_token": GOOD_TOKEN,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        login_resp.status(),
        StatusCode::OK,
        "{:?}",
        login_resp.text().await
    );
    let login_body: Value = login_resp.json().await.unwrap();
    assert!(login_body
        .get("access_token")
        .and_then(Value::as_str)
        .is_some());
}

// ── (b) missing/invalid token on signup -> rejected, no user created ───────

#[tokio::test]
async fn signup_rejects_missing_token_and_creates_no_user() {
    let mock = MockVerifier::spawn().await;
    let captcha = CaptchaConfig::new("env-secret", mock.verify_url(), ["signup", "login"]);
    let (server, _engine) = TestServer::spawn_with_signup_and_captcha(captcha, None).await;
    let client = reqwest::Client::new();

    // No captcha_token field at all.
    let missing = client
        .post(server.url("/auth/signup"))
        .json(&json!({ "username": "bob", "password": "hunter22" }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    let missing_body: Value = missing.json().await.unwrap();
    assert_eq!(missing_body["code"], "CAPTCHA_TOKEN_REQUIRED");

    // A present-but-wrong token.
    let wrong = client
        .post(server.url("/auth/signup"))
        .json(&json!({
            "username": "bob",
            "password": "hunter22",
            "captcha_token": BAD_TOKEN,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
    let wrong_body: Value = wrong.json().await.unwrap();
    assert_eq!(wrong_body["code"], "CAPTCHA_FAILED");

    // Prove no account was ever created: a login attempt for "bob" (even
    // with a *valid* captcha token) must fail with INVALID_CREDENTIALS, not
    // succeed against a half-created account.
    let login_probe = client
        .post(server.url("/auth/login"))
        .json(&json!({
            "username": "bob",
            "password": "hunter22",
            "captcha_token": GOOD_TOKEN,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(login_probe.status(), StatusCode::UNAUTHORIZED);
    let login_probe_body: Value = login_probe.json().await.unwrap();
    assert_eq!(login_probe_body["code"], "INVALID_CREDENTIALS");

    // Positive control: the exact same signup, this time with a valid
    // token, succeeds — proving the rejections above were the captcha gate,
    // not some other misconfiguration.
    let ok = client
        .post(server.url("/auth/signup"))
        .json(&json!({
            "username": "bob",
            "password": "hunter22",
            "captcha_token": GOOD_TOKEN,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK, "{:?}", ok.text().await);
}

// ── (c) bad/missing token on login -> rejected, no session ─────────────────

#[tokio::test]
async fn login_rejects_bad_or_missing_token_with_no_session() {
    let mock = MockVerifier::spawn().await;
    let captcha = CaptchaConfig::new("env-secret", mock.verify_url(), ["signup", "login"]);
    let (server, _engine) = TestServer::spawn_with_signup_and_captcha(captcha, None).await;
    let client = reqwest::Client::new();

    // Seed a real account first (valid-token signup, proven to work by (a)).
    let signup = client
        .post(server.url("/auth/signup"))
        .json(&json!({
            "username": "carol",
            "password": "swordfish99",
            "captcha_token": GOOD_TOKEN,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(signup.status(), StatusCode::OK);

    // Missing token.
    let missing = client
        .post(server.url("/auth/login"))
        .json(&json!({ "username": "carol", "password": "swordfish99" }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    let missing_body: Value = missing.json().await.unwrap();
    assert_eq!(missing_body["code"], "CAPTCHA_TOKEN_REQUIRED");
    assert!(missing_body.get("access_token").is_none());

    // Wrong token — the password itself is correct, proving the captcha
    // gate runs (and blocks) before credential verification.
    let wrong = client
        .post(server.url("/auth/login"))
        .json(&json!({
            "username": "carol",
            "password": "swordfish99",
            "captcha_token": BAD_TOKEN,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
    let wrong_body: Value = wrong.json().await.unwrap();
    assert_eq!(wrong_body["code"], "CAPTCHA_FAILED");
    assert!(wrong_body.get("access_token").is_none());
    assert!(wrong_body.get("refresh_token").is_none());

    // Valid token — a positive control proving the account is real and
    // reachable once the gate passes.
    let ok = client
        .post(server.url("/auth/login"))
        .json(&json!({
            "username": "carol",
            "password": "swordfish99",
            "captcha_token": GOOD_TOKEN,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK, "{:?}", ok.text().await);
}

// ── (d) disabled by default -> unconditional back-compat ───────────────────

#[tokio::test]
async fn captcha_disabled_by_default_is_back_compat() {
    // The plain `spawn_with_dev_login_and_signup` helper builds its
    // `AppState` with `CaptchaConfig::disabled()` (the `AppState::with_config`
    // default) — no captcha wiring at all, exactly like every pre-item-131
    // test in this suite.
    let server = TestServer::spawn_with_dev_login_and_signup().await;
    let client = reqwest::Client::new();

    let signup = client
        .post(server.url("/auth/signup"))
        .json(&json!({ "username": "dave", "password": "no-captcha-needed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(signup.status(), StatusCode::OK, "{:?}", signup.text().await);

    let login = client
        .post(server.url("/auth/login"))
        .json(&json!({ "username": "dave", "password": "no-captcha-needed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK, "{:?}", login.text().await);
    let login_body: Value = login.json().await.unwrap();
    assert!(login_body
        .get("access_token")
        .and_then(Value::as_str)
        .is_some());
}

// ── (e) secret resolution: vault first, env fallback ────────────────────────

#[tokio::test]
async fn secret_resolves_from_vault_when_stored_else_falls_back_to_env() {
    let mock = MockVerifier::spawn().await;
    let captcha = CaptchaConfig::new("env-secret-value", mock.verify_url(), ["signup"]);
    let (server, engine) =
        TestServer::spawn_with_signup_and_captcha(captcha, Some(TEST_MASTER_KEY_HEX)).await;
    let client = reqwest::Client::new();

    // No vault secret stored yet -> falls back to the env-sourced value.
    assert!(engine
        .get_secret(CAPTCHA_SECRET_NAME.to_string())
        .await
        .unwrap()
        .is_none());
    let resp1 = client
        .post(server.url("/auth/signup"))
        .json(&json!({
            "username": "erin",
            "password": "pw-erin",
            "captcha_token": GOOD_TOKEN,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK, "{:?}", resp1.text().await);
    assert_eq!(
        mock.last_captured_secret().as_deref(),
        Some("env-secret-value")
    );

    // Store a vault secret; it must now be what actually reaches the
    // verifier, not the env value baked into `CaptchaConfig::new`.
    engine
        .set_secret(
            CAPTCHA_SECRET_NAME.to_string(),
            "vault-secret-value".to_string(),
        )
        .await
        .unwrap();
    let resp2 = client
        .post(server.url("/auth/signup"))
        .json(&json!({
            "username": "frank",
            "password": "pw-frank",
            "captcha_token": GOOD_TOKEN,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK, "{:?}", resp2.text().await);
    assert_eq!(
        mock.last_captured_secret().as_deref(),
        Some("vault-secret-value"),
        "the vault-stored secret must win over the env-sourced value once stored"
    );
}
