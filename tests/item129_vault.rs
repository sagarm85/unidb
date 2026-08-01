// Item 129 (Workstream I3) — encrypt-at-rest secrets vault, OAuth wiring.
//
// Pure-crypto coverage ((a) encrypt/decrypt roundtrip, (b) wrong-key/
// tampered-ciphertext failure, (c) no-plaintext-in-Debug-or-persisted-JSON)
// lives as `#[cfg(test)]` unit tests in `src/vault.rs` and
// `src/authz/mod.rs` (run by `cargo test --lib`, no `server` feature
// needed — the vault itself is core control-plane state, not server-gated).
// This file covers the two acceptance items that specifically need the HTTP
// server + the OAuth callback route:
//   (d) OAuth reads a vault-stored client secret, and correctly falls back
//       to the env var when no vault secret is stored for that provider.
//   (e) no master key configured → vault disabled, existing (env-only)
//       OAuth behavior is unchanged.
//
// No real provider secrets and no real network: a tiny local mock OAuth
// provider stands in for Google/GitHub (same shape as `item128_oauth.rs`),
// capturing whatever `client_secret` value the exchange request actually
// carried so the test can assert *which* source (vault vs env) was used.

#![cfg(feature = "server")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{extract::State as AxumState, routing::get, Form, Json, Router};
use reqwest::StatusCode;
use serde_json::{json, Value};
use unidb::server::oauth::{OAuthConfig, OAuthProviderConfig};

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

/// A 32-byte AES-256 test master key (64 hex chars) — test-only, never a
/// real deployment key.
const TEST_MASTER_KEY_HEX: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[derive(Clone, Default)]
struct CapturedSecret(Arc<Mutex<Option<String>>>);

async fn mock_authorize() -> &'static str {
    "ok"
}

/// Records whatever `client_secret` form field the exchange request sent,
/// then returns a canned access token — same "no assertion baked into the
/// mock, the test asserts afterward" shape `item128_oauth.rs` uses, just
/// capturing one extra field.
async fn mock_token(
    AxumState(captured): AxumState<CapturedSecret>,
    Form(params): Form<HashMap<String, String>>,
) -> Json<Value> {
    *captured.0.lock().unwrap() = params.get("client_secret").cloned();
    Json(json!({ "access_token": "mock-access-token", "token_type": "bearer" }))
}

async fn mock_userinfo() -> Json<Value> {
    Json(json!({ "id": "provider-uid-i3", "email": "i3@example.com" }))
}

struct MockProvider {
    addr: std::net::SocketAddr,
    captured: CapturedSecret,
    _task: tokio::task::JoinHandle<()>,
}

impl MockProvider {
    async fn spawn() -> Self {
        let captured = CapturedSecret::default();
        let router = Router::new()
            .route("/authorize", get(mock_authorize))
            .route("/token", axum::routing::post(mock_token))
            .route("/userinfo", get(mock_userinfo))
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

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn last_captured_secret(&self) -> Option<String> {
        self.captured.0.lock().unwrap().clone()
    }
}

fn no_redirect_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Drive `GET /auth/oauth/<provider>/authorize` and return the `state` query
/// parameter, exactly like `item128_oauth.rs`'s helper of the same shape.
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
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let url = reqwest::Url::parse(&location).unwrap();
    url.query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.to_string())
        .expect("authorize redirect must carry a state param")
}

fn provider_config_for(mock: &MockProvider, env_secret: &str) -> OAuthProviderConfig {
    OAuthProviderConfig::new(
        "test-client-id",
        env_secret,
        "http://localhost/callback",
        format!("{}/authorize", mock.base_url()),
        format!("{}/token", mock.base_url()),
        format!("{}/userinfo", mock.base_url()),
        "openid email",
    )
}

fn oauth_config_with(provider: &str, cfg: OAuthProviderConfig) -> OAuthConfig {
    let mut providers = HashMap::new();
    providers.insert(provider.to_string(), cfg);
    OAuthConfig::from_providers(providers)
}

/// (d), vault branch: a secret stored under `oauth.<provider>.client_secret`
/// is what actually reaches the provider's token endpoint — not the env
/// value baked into `OAuthProviderConfig` at startup.
#[tokio::test]
async fn oauth_token_exchange_uses_the_vault_secret_when_stored() {
    let mock = MockProvider::spawn().await;
    let cfg = provider_config_for(&mock, "env-value-must-be-ignored");
    let oauth = oauth_config_with("google", cfg);
    let (server, engine) =
        TestServer::spawn_with_dev_login_oauth_and_engine(oauth, Some(TEST_MASTER_KEY_HEX)).await;

    engine
        .set_secret(
            unidb::server::oauth::oauth_secret_name("google"),
            "vault-secret-value".to_string(),
        )
        .await
        .unwrap();

    let client = no_redirect_client();
    let state = authorize_and_extract_state(&client, &server, "google").await;
    let resp = client
        .get(server.url(&format!(
            "/auth/oauth/google/callback?code=irrelevant&state={state}"
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.text().await);

    assert_eq!(
        mock.last_captured_secret().as_deref(),
        Some("vault-secret-value"),
        "token exchange must use the vault-stored secret, not the env value"
    );
}

/// (d), env-fallback branch: with no secret ever stored in the vault for
/// this provider, resolution falls back to the env-sourced value exactly
/// like before item 129 shipped — proven with the vault itself *enabled*
/// (a master key present), so this specifically exercises the "not in the
/// vault" branch of the resolver, not merely "vault disabled".
#[tokio::test]
async fn oauth_token_exchange_falls_back_to_env_secret_when_not_in_vault() {
    let mock = MockProvider::spawn().await;
    let cfg = provider_config_for(&mock, "env-secret-value");
    let oauth = oauth_config_with("google", cfg);
    let (server, engine) =
        TestServer::spawn_with_dev_login_oauth_and_engine(oauth, Some(TEST_MASTER_KEY_HEX)).await;
    // Vault is enabled but nothing was ever stored for this provider.
    assert!(engine
        .get_secret(unidb::server::oauth::oauth_secret_name("google"))
        .await
        .unwrap()
        .is_none());

    let client = no_redirect_client();
    let state = authorize_and_extract_state(&client, &server, "google").await;
    let resp = client
        .get(server.url(&format!(
            "/auth/oauth/google/callback?code=irrelevant&state={state}"
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.text().await);

    assert_eq!(
        mock.last_captured_secret().as_deref(),
        Some("env-secret-value"),
    );
}

/// (e): with no `UNIDB_MASTER_KEY` at all, the vault is disabled — OAuth
/// keeps working exactly as it did before item 129 (env secret only), and
/// attempting to store a secret fails clearly rather than silently writing
/// plaintext or panicking the server.
#[tokio::test]
async fn no_master_key_vault_disabled_oauth_behavior_unchanged() {
    let mock = MockProvider::spawn().await;
    let cfg = provider_config_for(&mock, "env-secret-value");
    let oauth = oauth_config_with("google", cfg);
    let (server, engine) = TestServer::spawn_with_dev_login_oauth_and_engine(oauth, None).await;

    // Attempting to store a secret with the vault disabled must fail
    // closed, never silently succeed with a plaintext write.
    assert!(engine
        .set_secret("oauth.google.client_secret".to_string(), "x".to_string())
        .await
        .is_err());

    let client = no_redirect_client();
    let state = authorize_and_extract_state(&client, &server, "google").await;
    let resp = client
        .get(server.url(&format!(
            "/auth/oauth/google/callback?code=irrelevant&state={state}"
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.text().await);
    assert_eq!(
        mock.last_captured_secret().as_deref(),
        Some("env-secret-value"),
        "with the vault disabled, OAuth must behave exactly as it did before item 129"
    );
}
