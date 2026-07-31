// Item 128 (Workstream D1) — OAuth 2.0 Authorization Code + PKCE social
// login (Google/GitHub-shaped, provider-agnostic).
//
// No real provider secrets and no real network: a tiny local axum "mock
// OAuth provider" (`/authorize` no-op, `/token` canned access token,
// `/userinfo` canned id+email) stands in for Google/GitHub, and a test
// provider's authorize/token/userinfo URLs are pointed at it via
// `OAuthProviderConfig::new` (the same override seam `UNIDB_OAUTH_
// <PROVIDER>_AUTHORIZE_URL`/`_TOKEN_URL`/`_USERINFO_URL` give a real
// deployment for pointing at a staging IdP). Both the mock provider and the
// unidb-under-test server bind to `127.0.0.1:0` (ephemeral loopback ports
// only) — no outbound network reaches anywhere real.
//
// Test matrix (mirrors the task spec's acceptance list):
//   (a) authorize_then_callback_issues_a_session_and_links_identity
//   (b) second_login_with_same_identity_reuses_the_same_user
//   (c) callback_rejects_unknown_and_replayed_state
//   (d) unconfigured_provider_404s
//   (e) provider_token_exchange_failure_returns_clean_error_no_session
//       + provider_unreachable_returns_502_no_session
//       + provider_denied_consent_returns_400_no_session (bonus)

#![cfg(feature = "server")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{extract::State as AxumState, response::IntoResponse, routing::get, Json, Router};
use reqwest::StatusCode;
use serde_json::{json, Value};
use unidb::server::oauth::{OAuthConfig, OAuthProviderConfig};

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

// ── a tiny local mock OAuth provider (no real network, no real secrets) ────

#[derive(Clone)]
struct MockProviderState {
    inner: Arc<Mutex<MockProviderInner>>,
}

struct MockProviderInner {
    provider_user_id: String,
    email: Option<String>,
    access_token: String,
    fail_token_exchange: bool,
}

impl MockProviderState {
    fn new(provider_user_id: &str, email: &str) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockProviderInner {
                provider_user_id: provider_user_id.to_string(),
                email: Some(email.to_string()),
                access_token: format!("mock-access-token-for-{provider_user_id}"),
                fail_token_exchange: false,
            })),
        }
    }

    fn set_fail_token_exchange(&self, fail: bool) {
        self.inner.lock().unwrap().fail_token_exchange = fail;
    }
}

/// `GET /authorize` — no-op (a real provider would render a consent screen
/// here; nothing in the flow under test ever fetches this URL over HTTP, it
/// only needs to exist as a syntactically valid target for the redirect
/// unidb's own `/authorize` route builds).
async fn mock_authorize() -> &'static str {
    "ok"
}

/// `POST /token` — returns a canned access token, or a canned `400
/// invalid_grant` when [`MockProviderState::set_fail_token_exchange`] was
/// toggled on (drives the token-exchange-failure test).
async fn mock_token(AxumState(state): AxumState<MockProviderState>) -> impl IntoResponse {
    let inner = state.inner.lock().unwrap();
    if inner.fail_token_exchange {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_grant" })),
        );
    }
    (
        axum::http::StatusCode::OK,
        Json(json!({
            "access_token": inner.access_token,
            "token_type": "bearer",
            "expires_in": 3600,
        })),
    )
}

/// `GET /userinfo` — returns the canned `{id, email}` when `Authorization:
/// Bearer <access_token>` matches the token this instance handed out;
/// `401` otherwise.
async fn mock_userinfo(
    AxumState(state): AxumState<MockProviderState>,
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
    let mut body = json!({ "id": inner.provider_user_id });
    if let Some(email) = &inner.email {
        body["email"] = json!(email);
    }
    (axum::http::StatusCode::OK, Json(body))
}

struct MockProvider {
    addr: std::net::SocketAddr,
    state: MockProviderState,
    _task: tokio::task::JoinHandle<()>,
}

impl MockProvider {
    async fn spawn(provider_user_id: &str, email: &str) -> Self {
        let state = MockProviderState::new(provider_user_id, email);
        let router = Router::new()
            .route("/authorize", get(mock_authorize))
            .route("/token", axum::routing::post(mock_token))
            .route("/userinfo", get(mock_userinfo))
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
    fn authorize_url(&self) -> String {
        format!("{}/authorize", self.base_url())
    }
    fn token_url(&self) -> String {
        format!("{}/token", self.base_url())
    }
    fn userinfo_url(&self) -> String {
        format!("{}/userinfo", self.base_url())
    }
}

fn provider_config_for(mock: &MockProvider) -> OAuthProviderConfig {
    OAuthProviderConfig::new(
        "test-client-id",
        "test-client-secret",
        "http://localhost/callback",
        mock.authorize_url(),
        mock.token_url(),
        mock.userinfo_url(),
        "openid email",
    )
}

fn oauth_config_with(provider: &str, mock: &MockProvider) -> OAuthConfig {
    let mut providers = HashMap::new();
    providers.insert(provider.to_string(), provider_config_for(mock));
    OAuthConfig::from_providers(providers)
}

/// A client that does NOT auto-follow redirects, so the test can inspect
/// `GET /auth/oauth/<provider>/authorize`'s raw `302` response and its
/// `Location` header (rather than transparently landing on the mock
/// provider's `/authorize` no-op).
fn no_redirect_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn whoami(client: &reqwest::Client, server: &TestServer, token: &str) -> Value {
    client
        .get(server.url("/auth/whoami"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Drive `GET /auth/oauth/<provider>/authorize`, assert it's a real PKCE
/// redirect, and return the `state` query parameter for the caller to
/// redeem at the callback route.
async fn authorize_and_extract_state(
    client: &reqwest::Client,
    server: &TestServer,
    provider: &str,
) -> String {
    let resp = client
        .get(server.url(&format!("/auth/oauth/{provider}/authorize")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .expect("authorize must redirect with a Location header")
        .to_str()
        .unwrap()
        .to_string();
    let url = reqwest::Url::parse(&location).expect("Location must be a valid absolute URL");
    assert!(
        url.query_pairs()
            .any(|(k, v)| k == "response_type" && v == "code"),
        "authorize URL must request an authorization code"
    );
    assert!(
        url.query_pairs()
            .any(|(k, v)| k == "code_challenge_method" && v == "S256"),
        "authorize URL must carry the PKCE S256 code_challenge_method"
    );
    assert!(
        url.query_pairs().any(|(k, _)| k == "code_challenge"),
        "authorize URL must carry a PKCE code_challenge"
    );
    url.query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.to_string())
        .expect("authorize URL must carry a state param")
}

// ── (a) authorize -> callback -> a valid unidb session, identity linked ────

#[tokio::test]
async fn authorize_then_callback_issues_a_session_and_links_identity() {
    let mock = MockProvider::spawn("mock-uid-1", "alice@example.com").await;
    let server =
        TestServer::spawn_with_dev_login_and_oauth(oauth_config_with("google", &mock)).await;
    let client = no_redirect_client();
    let follow_client = reqwest::Client::new();

    let state = authorize_and_extract_state(&client, &server, "google").await;

    let resp = client
        .get(server.url(&format!(
            "/auth/oauth/google/callback?code=any-provider-code&state={state}"
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(body.get("access_token").and_then(Value::as_str).is_some());
    assert!(body.get("refresh_token").and_then(Value::as_str).is_some());
    assert_eq!(body["expires_in"], 3600);

    let access_token = body["access_token"].as_str().unwrap();
    let who = whoami(&follow_client, &server, access_token).await;
    assert_eq!(who["user"], "oauth_google_mock-uid-1");
    assert_eq!(who["is_superuser"], false);
    // oauth_link_or_create just registered the server's first user, so the
    // engine has left open/bootstrap mode.
    assert_eq!(who["open_mode"], false);
}

// ── (b) a second login with the same identity reuses the same user ─────────

#[tokio::test]
async fn second_login_with_same_identity_reuses_the_same_user() {
    let mock = MockProvider::spawn("mock-uid-2", "bob@example.com").await;
    let server =
        TestServer::spawn_with_dev_login_and_oauth(oauth_config_with("google", &mock)).await;
    let client = no_redirect_client();
    let follow_client = reqwest::Client::new();

    let state1 = authorize_and_extract_state(&client, &server, "google").await;
    let resp1 = client
        .get(server.url(&format!(
            "/auth/oauth/google/callback?code=first-code&state={state1}"
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let body1: Value = resp1.json().await.unwrap();
    let who1 = whoami(
        &follow_client,
        &server,
        body1["access_token"].as_str().unwrap(),
    )
    .await;

    // A fresh authorize/callback round trip (a brand-new state — the first
    // one is already single-use-consumed) for the exact same provider
    // identity.
    let state2 = authorize_and_extract_state(&client, &server, "google").await;
    let resp2 = client
        .get(server.url(&format!(
            "/auth/oauth/google/callback?code=second-code&state={state2}"
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let body2: Value = resp2.json().await.unwrap();
    let who2 = whoami(
        &follow_client,
        &server,
        body2["access_token"].as_str().unwrap(),
    )
    .await;

    assert_eq!(who1["user"], who2["user"]);
    // Two independent sessions (different refresh tokens) for the same user.
    assert_ne!(body1["refresh_token"], body2["refresh_token"]);
}

// ── (c) state mismatch/replay -> 401, no oracle ─────────────────────────────

#[tokio::test]
async fn callback_rejects_unknown_and_replayed_state() {
    let mock = MockProvider::spawn("mock-uid-3", "carol@example.com").await;
    let server =
        TestServer::spawn_with_dev_login_and_oauth(oauth_config_with("google", &mock)).await;
    let client = no_redirect_client();

    // Unknown/garbage state never issued by this server.
    let resp = client
        .get(server.url("/auth/oauth/google/callback?code=x&state=not-a-real-state"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "OAUTH_STATE_INVALID");

    // A real state, redeemed once successfully...
    let state = authorize_and_extract_state(&client, &server, "google").await;
    let ok = client
        .get(server.url(&format!("/auth/oauth/google/callback?code=c&state={state}")))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);

    // ...then replayed: must fail, even though the state string is genuine.
    let replay = client
        .get(server.url(&format!("/auth/oauth/google/callback?code=c&state={state}")))
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    let replay_body: Value = replay.json().await.unwrap();
    assert_eq!(replay_body["code"], "OAUTH_STATE_INVALID");
}

#[tokio::test]
async fn callback_rejects_state_issued_for_a_different_provider() {
    let mock_google = MockProvider::spawn("mock-uid-3b", "carol2@example.com").await;
    let mock_github = MockProvider::spawn("mock-uid-3c", "carol3@example.com").await;
    let mut providers = HashMap::new();
    providers.insert("google".to_string(), provider_config_for(&mock_google));
    providers.insert("github".to_string(), provider_config_for(&mock_github));
    let server =
        TestServer::spawn_with_dev_login_and_oauth(OAuthConfig::from_providers(providers)).await;
    let client = no_redirect_client();

    // Mint a state for "google"...
    let state = authorize_and_extract_state(&client, &server, "google").await;
    // ...and try to redeem it against the "github" callback route.
    let resp = client
        .get(server.url(&format!("/auth/oauth/github/callback?code=c&state={state}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "OAUTH_STATE_INVALID");
}

// ── (d) an unconfigured provider -> 404 ─────────────────────────────────────

#[tokio::test]
async fn unconfigured_provider_404s() {
    let server = TestServer::spawn_with_dev_login_and_oauth(OAuthConfig::empty()).await;
    let client = no_redirect_client();

    let resp = client
        .get(server.url("/auth/oauth/google/authorize"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp2 = client
        .get(server.url("/auth/oauth/google/callback?code=x&state=y"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::NOT_FOUND);
}

// ── (e) provider token-exchange failure -> clean 4xx/5xx, no session ───────

#[tokio::test]
async fn provider_token_exchange_failure_returns_clean_error_no_session() {
    let mock = MockProvider::spawn("mock-uid-5", "eve@example.com").await;
    mock.state.set_fail_token_exchange(true);
    let server =
        TestServer::spawn_with_dev_login_and_oauth(oauth_config_with("google", &mock)).await;
    let client = no_redirect_client();

    let state = authorize_and_extract_state(&client, &server, "google").await;
    let resp = client
        .get(server.url(&format!("/auth/oauth/google/callback?code=c&state={state}")))
        .send()
        .await
        .unwrap();
    // The mock provider explicitly rejected (400 invalid_grant) -> a clean,
    // uniform 401, and definitely no token pair in the body.
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "OAUTH_TOKEN_EXCHANGE_FAILED");
    assert!(body.get("access_token").is_none());
    assert!(body.get("refresh_token").is_none());
}

#[tokio::test]
async fn provider_unreachable_returns_502_no_session() {
    let mock = MockProvider::spawn("mock-uid-6", "frank@example.com").await;
    // Point token_url at a loopback port nothing listens on — a real
    // connection-refused failure, still zero real network (127.0.0.1 only).
    let mut providers = HashMap::new();
    providers.insert(
        "google".to_string(),
        OAuthProviderConfig::new(
            "test-client-id",
            "test-client-secret",
            "http://localhost/callback",
            mock.authorize_url(),
            "http://127.0.0.1:1/token",
            mock.userinfo_url(),
            "openid email",
        ),
    );
    let server =
        TestServer::spawn_with_dev_login_and_oauth(OAuthConfig::from_providers(providers)).await;
    let client = no_redirect_client();

    let state = authorize_and_extract_state(&client, &server, "google").await;
    let resp = client
        .get(server.url(&format!("/auth/oauth/google/callback?code=c&state={state}")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "OAUTH_PROVIDER_UNAVAILABLE");
    assert!(body.get("access_token").is_none());
}

#[tokio::test]
async fn provider_denied_consent_returns_400_no_session() {
    let mock = MockProvider::spawn("mock-uid-7", "grace@example.com").await;
    let server =
        TestServer::spawn_with_dev_login_and_oauth(oauth_config_with("google", &mock)).await;
    let client = no_redirect_client();

    let state = authorize_and_extract_state(&client, &server, "google").await;
    let resp = client
        .get(server.url(&format!(
            "/auth/oauth/google/callback?error=access_denied&state={state}"
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "OAUTH_PROVIDER_DENIED");
    assert!(body.get("access_token").is_none());
}
