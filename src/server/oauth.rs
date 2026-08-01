//! OAuth 2.0 Authorization Code + PKCE, provider-agnostic social login (item
//! 128, Workstream D1 of the [Supabase parity
//! roadmap](../../../docs/backlog/120_supabase_parity_roadmap.md)). Google
//! and GitHub ship as the two recognized providers; the flow itself has
//! nothing provider-specific baked into it beyond three URLs + a scope
//! string, so a third provider is a config addition, not a code change.
//!
//! **Flow** (see `handlers.rs::get_oauth_authorize`/`get_oauth_callback` for
//! the HTTP-level detail):
//! 1. `GET /auth/oauth/<provider>/authorize` mints a CSRF `state` + PKCE
//!    `code_verifier` ([`crate::authz::RoleStore::create_oauth_state`]),
//!    derives the PKCE `code_challenge` ([`code_challenge_s256`]), and
//!    redirects to the provider's authorize URL
//!    ([`build_authorize_url`]).
//! 2. `GET /auth/oauth/<provider>/callback?code=&state=` validates `state`
//!    (single-use, TTL-bounded, provider-pinned —
//!    [`crate::authz::RoleStore::verify_oauth_state`]), exchanges `code` for
//!    a provider access token with the matching PKCE verifier
//!    ([`exchange_code_for_token`]), fetches the provider's userinfo
//!    ([`fetch_userinfo`]), then resolves `(provider, provider_user_id)` to
//!    a unidb user ([`crate::authz::RoleStore::oauth_link_or_create`]) and
//!    issues a normal session through the exact same `issue_token_pair`
//!    helper every other login path uses.
//!
//! **Config.** Per provider: `UNIDB_OAUTH_<PROVIDER>_CLIENT_ID` /
//! `_CLIENT_SECRET` / `_REDIRECT_URI` (required — a provider missing any of
//! these is simply absent from [`OAuthConfig`], so its routes 404, same
//! posture as dev-login/signup being off by default) plus optional
//! `_AUTHORIZE_URL` / `_TOKEN_URL` / `_USERINFO_URL` / `_SCOPE` overrides —
//! real Google/GitHub endpoints are the built-in defaults; overriding lets a
//! test point a provider at a local mock (see `tests/item128_oauth.rs`)
//! with zero real network and zero real provider secrets.
//!
//! **Secret handling.** [`ClientSecret`] is a small accessor wrapper — never
//! `Debug`-printed, never logged — around the raw secret string read from
//! the environment. `// I3: route through the vault when available` marks
//! the exact seam: the next item (I3, secrets vault) can make
//! [`ClientSecret::expose`] decrypt-on-read from a real vault without
//! touching any call site in this module or `handlers.rs`.
//!
//! **I3 update (item 129 — shipped):** [`resolve_client_secret`] is that
//! seam realized. At token-exchange time (`GET /auth/oauth/<provider>/
//! callback`, in `handlers.rs`), the resolver looks up the vault secret
//! named `oauth.<provider>.client_secret`
//! ([`crate::authz::RoleStore::get_secret`], via
//! [`crate::server::engine_handle::EngineHandle::get_secret`]): if one is
//! stored, its decrypted value is used; otherwise the
//! `UNIDB_OAUTH_<PROVIDER>_CLIENT_SECRET` env value captured in
//! [`OAuthProviderConfig`] at startup is used, exactly as before this item
//! shipped. A vault secret that WAS stored but can't be decrypted (vault
//! disabled/wrong key, tampered blob) is a hard error — never a silent
//! plaintext fallback to a possibly-stale/absent env value. With no
//! `UNIDB_MASTER_KEY` configured (vault disabled) and no vault secret ever
//! stored, resolution always takes the env branch — D1's original behavior
//! is unchanged byte-for-byte.
//!
//! Because `UNIDB_OAUTH_<PROVIDER>_CLIENT_SECRET` is optional as of item 129
//! (see [`OAuthConfig::from_env`]), an operator can configure a provider
//! **vault-only**: set `_CLIENT_ID`/`_REDIRECT_URI` in the environment (not
//! secret — fine to keep there) and store only the client secret via
//! `unidb-vault set oauth.<provider>.client_secret`. [`resolve_client_secret`]
//! errors clearly if resolution finds the secret in neither place.

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// A client secret, held behind a small accessor so the next item (I3,
/// secrets vault) can slot in vault-backed storage/decryption without
/// touching any call site. Never `Debug`-printed (see the manual impl
/// below) and never logged — `handlers.rs`/`oauth.rs` only ever pass it
/// straight into the outbound HTTPS request body to the provider's token
/// endpoint, never into a `tracing::*!` call or an HTTP response.
#[derive(Clone)]
pub struct ClientSecret(String);

impl ClientSecret {
    fn new(raw: String) -> Self {
        Self(raw)
    }

    /// The plaintext env-sourced value, or `""` when
    /// `UNIDB_OAUTH_<PROVIDER>_CLIENT_SECRET` was unset at startup (item
    /// 129: the env var is now optional — see [`OAuthConfig::from_env`]).
    /// Callers resolving the *effective* client secret should go through
    /// [`resolve_client_secret`] (vault-first, this as the fallback), not
    /// this accessor directly.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ClientSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted>")
    }
}

/// One provider's OAuth configuration, resolved once at startup (or once
/// per test server). `Debug` redacts [`Self::client_secret`] — see
/// [`ClientSecret`].
#[derive(Clone)]
pub struct OAuthProviderConfig {
    pub client_id: String,
    client_secret: ClientSecret,
    pub redirect_uri: String,
    pub authorize_url: String,
    pub token_url: String,
    pub userinfo_url: String,
    pub scope: String,
}

impl std::fmt::Debug for OAuthProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthProviderConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret)
            .field("redirect_uri", &self.redirect_uri)
            .field("authorize_url", &self.authorize_url)
            .field("token_url", &self.token_url)
            .field("userinfo_url", &self.userinfo_url)
            .field("scope", &self.scope)
            .finish()
    }
}

impl OAuthProviderConfig {
    /// The env-sourced client secret only (may be `""` — see
    /// [`ClientSecret::expose`]). Never logged, never returned in any HTTP
    /// response. Prefer [`resolve_client_secret`] at token-exchange time,
    /// which additionally checks the vault first (item 129, I3).
    pub fn client_secret(&self) -> &str {
        self.client_secret.expose()
    }

    /// Construct directly (used by tests to point a provider at a local
    /// mock server without going through environment variables).
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        redirect_uri: impl Into<String>,
        authorize_url: impl Into<String>,
        token_url: impl Into<String>,
        userinfo_url: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: ClientSecret::new(client_secret.into()),
            redirect_uri: redirect_uri.into(),
            authorize_url: authorize_url.into(),
            token_url: token_url.into(),
            userinfo_url: userinfo_url.into(),
            scope: scope.into(),
        }
    }
}

const GOOGLE_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";
const GOOGLE_DEFAULT_SCOPE: &str = "openid email profile";

const GITHUB_AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_USERINFO_URL: &str = "https://api.github.com/user";
const GITHUB_DEFAULT_SCOPE: &str = "read:user user:email";

/// The two recognized provider names and their real-world default endpoint
/// URLs + scope. `None` for anything else — `OAuthConfig::from_env` only
/// ever looks these two up.
fn default_urls(
    provider: &str,
) -> Option<(&'static str, &'static str, &'static str, &'static str)> {
    match provider {
        "google" => Some((
            GOOGLE_AUTHORIZE_URL,
            GOOGLE_TOKEN_URL,
            GOOGLE_USERINFO_URL,
            GOOGLE_DEFAULT_SCOPE,
        )),
        "github" => Some((
            GITHUB_AUTHORIZE_URL,
            GITHUB_TOKEN_URL,
            GITHUB_USERINFO_URL,
            GITHUB_DEFAULT_SCOPE,
        )),
        _ => None,
    }
}

/// Every provider this server is configured for, keyed by lowercase name
/// ("google"/"github"), plus the shared HTTP client used for outbound
/// token-exchange/userinfo calls. Empty by default — "works safely with
/// zero providers configured" (item 128's stated acceptance): every
/// `/auth/oauth/*` route 404s for a provider not present here.
#[derive(Clone)]
pub struct OAuthConfig {
    providers: HashMap<String, OAuthProviderConfig>,
    http: reqwest::Client,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self::empty()
    }
}

impl OAuthConfig {
    pub fn empty() -> Self {
        Self {
            providers: HashMap::new(),
            http: build_http_client(),
        }
    }

    pub fn from_providers(providers: HashMap<String, OAuthProviderConfig>) -> Self {
        Self {
            providers,
            http: build_http_client(),
        }
    }

    /// Build from `UNIDB_OAUTH_<PROVIDER>_CLIENT_ID` / `_CLIENT_SECRET` /
    /// `_REDIRECT_URI` (+ optional `_AUTHORIZE_URL` / `_TOKEN_URL` /
    /// `_USERINFO_URL` / `_SCOPE` overrides) for "google" and "github". A
    /// provider missing `_CLIENT_ID` or `_REDIRECT_URI` is simply absent
    /// from the resulting config (its routes then 404) — unchanged from
    /// before item 129.
    ///
    /// **Item 129 (I3) change:** `_CLIENT_SECRET` is now *optional* at this
    /// layer — a provider with `_CLIENT_ID`/`_REDIRECT_URI` set but no
    /// `_CLIENT_SECRET` is still configured (its routes are live), so an
    /// operator can supply the secret exclusively via the vault
    /// (`unidb-vault set oauth.<provider>.client_secret`) instead of the
    /// environment. If the secret ends up in *neither* place,
    /// [`resolve_client_secret`] fails clearly at token-exchange time
    /// (`GET /auth/oauth/<provider>/callback`) rather than at startup —
    /// exactly mirroring how a bad/expired secret already only surfaces at
    /// exchange time (the provider rejects it), not at config-build time.
    pub fn from_env() -> Self {
        let mut providers = HashMap::new();
        for name in ["google", "github"] {
            if let Some(cfg) = provider_from_env(name) {
                providers.insert(name.to_string(), cfg);
            }
        }
        Self::from_providers(providers)
    }

    pub fn get(&self, provider: &str) -> Option<&OAuthProviderConfig> {
        self.providers.get(provider)
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }
}

fn env_var(provider_upper: &str, suffix: &str) -> Option<String> {
    std::env::var(format!("UNIDB_OAUTH_{provider_upper}_{suffix}"))
        .ok()
        .filter(|s| !s.is_empty())
}

fn provider_from_env(provider: &str) -> Option<OAuthProviderConfig> {
    let upper = provider.to_ascii_uppercase();
    let client_id = env_var(&upper, "CLIENT_ID")?;
    let redirect_uri = env_var(&upper, "REDIRECT_URI")?;
    // Item 129 (I3): optional — `resolve_client_secret` falls back to the
    // vault when this is absent/empty. `unwrap_or_default` gives `""`,
    // which `resolve_client_secret` treats identically to "not configured
    // in env" (see its doc comment).
    let client_secret = env_var(&upper, "CLIENT_SECRET").unwrap_or_default();
    // Only "google"/"github" are ever passed in here (see `from_env`'s
    // fixed loop), so this always resolves.
    let (def_authorize, def_token, def_userinfo, def_scope) = default_urls(provider)?;
    let authorize_url =
        env_var(&upper, "AUTHORIZE_URL").unwrap_or_else(|| def_authorize.to_string());
    let token_url = env_var(&upper, "TOKEN_URL").unwrap_or_else(|| def_token.to_string());
    let userinfo_url = env_var(&upper, "USERINFO_URL").unwrap_or_else(|| def_userinfo.to_string());
    let scope = env_var(&upper, "SCOPE").unwrap_or_else(|| def_scope.to_string());
    Some(OAuthProviderConfig::new(
        client_id,
        client_secret,
        redirect_uri,
        authorize_url,
        token_url,
        userinfo_url,
        scope,
    ))
}

/// Outbound HTTP client for token-exchange/userinfo calls, with a bounded
/// timeout so a slow/unresponsive provider fails the request instead of
/// hanging it — never `unwrap`/`expect` (CLAUDE.md's coding conventions):
/// falls back to an un-configured default client (no explicit timeout) in
/// the vanishingly unlikely case the builder itself fails, rather than
/// panicking a production server at startup over an HTTP client config
/// error.
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// PKCE (RFC 7636) `code_challenge` for the `S256` method: base64url
/// (no padding) of the SHA-256 digest of the ASCII `code_verifier`.
pub fn code_challenge_s256(code_verifier: &str) -> String {
    use base64::Engine;
    let digest = Sha256::digest(code_verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Build the provider's authorize-endpoint URL with the standard
/// Authorization-Code + PKCE query parameters.
pub fn build_authorize_url(cfg: &OAuthProviderConfig, state: &str, code_challenge: &str) -> String {
    let mut qs = form_urlencoded::Serializer::new(String::new());
    qs.append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", &cfg.redirect_uri)
        .append_pair("scope", &cfg.scope)
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("response_type", "code");
    format!("{}?{}", cfg.authorize_url, qs.finish())
}

/// Vault secret name for `provider`'s OAuth client secret (item 129, I3):
/// `oauth.<provider>.client_secret`. `pub` so both this module and callers
/// wiring `unidb-vault set` know the exact convention without duplicating
/// the literal.
pub fn oauth_secret_name(provider: &str) -> String {
    format!("oauth.{provider}.client_secret")
}

/// Resolve the *effective* client secret for `provider` (item 129, I3):
/// vault first ([`oauth_secret_name`]), falling back to the
/// `UNIDB_OAUTH_<PROVIDER>_CLIENT_SECRET` env value already captured in
/// `cfg` when no vault secret is stored — see the module doc's "I3 update"
/// section for the exact order. `Err` (never a silent empty/placeholder
/// secret) when the vault lookup itself fails (disabled vault holding a
/// stored secret, wrong/rotated key, tampered blob) or when the secret is
/// in neither place.
pub async fn resolve_client_secret(
    engine: &crate::server::engine_handle::EngineHandle,
    provider: &str,
    cfg: &OAuthProviderConfig,
) -> Result<String, OAuthError> {
    let name = oauth_secret_name(provider);
    match engine.get_secret(name.clone()).await {
        Ok(Some(secret)) => Ok(secret),
        Ok(None) => {
            let env_secret = cfg.client_secret();
            if env_secret.is_empty() {
                Err(OAuthError::ProviderUnavailable(format!(
                    "no client secret configured for OAuth provider '{provider}' — set it via \
                     `unidb-vault set {name}` or UNIDB_OAUTH_{}_CLIENT_SECRET",
                    provider.to_ascii_uppercase()
                )))
            } else {
                Ok(env_secret.to_string())
            }
        }
        Err(e) => Err(OAuthError::ProviderUnavailable(format!(
            "vault error resolving client secret for OAuth provider '{provider}': {e}"
        ))),
    }
}

/// A failure talking to the provider (network/HTTP layer), distinguished
/// from a provider-side rejection so `handlers.rs` can map each to the
/// right status code: unreachable/erroring provider → `502`; provider
/// explicitly rejected the exchange (bad code, bad verifier, expired,
/// revoked client) → `401`. Never carries the client secret or any live
/// provider access token — only human-readable diagnostic text.
#[derive(Debug)]
pub enum OAuthError {
    /// Could not reach the provider, the provider errored server-side, or
    /// its response was unparseable — maps to `502 OAUTH_PROVIDER_UNAVAILABLE`.
    ProviderUnavailable(String),
    /// The provider was reached and explicitly rejected the request (4xx) —
    /// maps to `401 OAUTH_TOKEN_EXCHANGE_FAILED`.
    ProviderRejected(String),
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Exchange an authorization `code` for a provider access token
/// (server-to-server `POST` to the provider's token endpoint), including the
/// PKCE `code_verifier` matching the `code_challenge` sent at authorize
/// time. Sends `Accept: application/json` explicitly — GitHub's token
/// endpoint replies form-encoded by default unless asked for JSON; Google
/// already replies JSON regardless, so the header is harmless there.
///
/// `client_secret` is the already-*resolved* value (item 129: vault-or-env
/// — see [`resolve_client_secret`]), not read from `cfg` here, so this
/// function itself stays vault-agnostic and synchronous-signature-shaped
/// (no `EngineHandle` dependency).
pub async fn exchange_code_for_token(
    client: &reqwest::Client,
    cfg: &OAuthProviderConfig,
    client_secret: &str,
    code: &str,
    code_verifier: &str,
) -> Result<String, OAuthError> {
    let params = [
        ("grant_type", "authorization_code"),
        ("client_id", cfg.client_id.as_str()),
        ("client_secret", client_secret),
        ("code", code),
        ("redirect_uri", cfg.redirect_uri.as_str()),
        ("code_verifier", code_verifier),
    ];
    let resp = client
        .post(&cfg.token_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&params)
        .send()
        .await
        .map_err(|e| OAuthError::ProviderUnavailable(format!("token request failed: {e}")))?;
    let status = resp.status();
    if status.is_server_error() {
        return Err(OAuthError::ProviderUnavailable(format!(
            "provider token endpoint returned {status}"
        )));
    }
    if !status.is_success() {
        return Err(OAuthError::ProviderRejected(format!(
            "provider rejected the authorization code (status {status})"
        )));
    }
    let body: TokenResponse = resp
        .json()
        .await
        .map_err(|e| OAuthError::ProviderUnavailable(format!("malformed token response: {e}")))?;
    Ok(body.access_token)
}

/// Minimal userinfo shape both Google's OIDC userinfo endpoint (`sub` +
/// `email`), GitHub's user endpoint (`id` + `email`), and the mock provider
/// in tests can populate. `id`/`sub` may be a JSON string or number
/// (providers differ) — [`UserInfo::provider_user_id`] normalizes either to
/// a `String`.
#[derive(Deserialize)]
struct UserInfo {
    #[serde(default)]
    sub: Option<serde_json::Value>,
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    email: Option<String>,
}

fn value_to_id_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl UserInfo {
    fn provider_user_id(&self) -> Option<String> {
        self.sub
            .as_ref()
            .and_then(value_to_id_string)
            .or_else(|| self.id.as_ref().and_then(value_to_id_string))
    }
}

/// Fetch `(provider_user_id, email)` from the provider's userinfo endpoint
/// using the access token from [`exchange_code_for_token`]. `email` is
/// `Option` — some providers (GitHub, when the user's email is private) omit
/// it — and is currently informational only (see the create-vs-link-by-email
/// design note on [`crate::authz::RoleStore::oauth_link_or_create`]): a
/// missing email never blocks login.
pub async fn fetch_userinfo(
    client: &reqwest::Client,
    cfg: &OAuthProviderConfig,
    access_token: &str,
) -> Result<(String, Option<String>), OAuthError> {
    let resp = client
        .get(&cfg.userinfo_url)
        .bearer_auth(access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| OAuthError::ProviderUnavailable(format!("userinfo request failed: {e}")))?;
    let status = resp.status();
    if status.is_server_error() {
        return Err(OAuthError::ProviderUnavailable(format!(
            "provider userinfo endpoint returned {status}"
        )));
    }
    if !status.is_success() {
        return Err(OAuthError::ProviderRejected(format!(
            "provider rejected the access token (status {status})"
        )));
    }
    let body: UserInfo = resp.json().await.map_err(|e| {
        OAuthError::ProviderUnavailable(format!("malformed userinfo response: {e}"))
    })?;
    let provider_user_id = body.provider_user_id().ok_or_else(|| {
        OAuthError::ProviderUnavailable(
            "userinfo response is missing an 'id'/'sub' field".to_string(),
        )
    })?;
    Ok((provider_user_id, body.email))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_challenge_s256_is_deterministic_and_url_safe() {
        // RFC 7636 Appendix B's worked example.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = code_challenge_s256(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        // No base64 padding, no '+'/'/' (base64url, not base64).
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
    }

    #[test]
    fn build_authorize_url_includes_pkce_and_state() {
        let cfg = OAuthProviderConfig::new(
            "cid",
            "csecret",
            "https://app.example/callback",
            "https://provider.example/authorize",
            "https://provider.example/token",
            "https://provider.example/userinfo",
            "openid email",
        );
        let url = build_authorize_url(&cfg, "the-state", "the-challenge");
        assert!(url.starts_with("https://provider.example/authorize?"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("state=the-state"));
        assert!(url.contains("code_challenge=the-challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("response_type=code"));
        // The client secret must never appear in the (browser-visible)
        // authorize URL.
        assert!(!url.contains("csecret"));
    }

    #[test]
    fn provider_config_debug_never_leaks_the_client_secret() {
        let cfg = OAuthProviderConfig::new(
            "cid",
            "super-secret-value",
            "https://app.example/callback",
            "https://provider.example/authorize",
            "https://provider.example/token",
            "https://provider.example/userinfo",
            "openid email",
        );
        let debug_str = format!("{cfg:?}");
        assert!(!debug_str.contains("super-secret-value"));
        assert!(debug_str.contains("redacted"));
    }

    #[test]
    fn from_env_absent_provider_is_not_configured() {
        // No UNIDB_OAUTH_GOOGLE_* vars set in this process by default.
        std::env::remove_var("UNIDB_OAUTH_GOOGLE_CLIENT_ID");
        std::env::remove_var("UNIDB_OAUTH_GOOGLE_CLIENT_SECRET");
        std::env::remove_var("UNIDB_OAUTH_GOOGLE_REDIRECT_URI");
        let cfg = OAuthConfig::from_env();
        assert!(cfg.get("google").is_none());
    }
}
