//! Provider-agnostic CAPTCHA / bot-protection verification for the
//! credential auth mutation routes (item 131, Workstream I of the [Supabase
//! parity roadmap](../../../docs/backlog/120_supabase_parity_roadmap.md),
//! I2). Complements the I1 rate limiter (`rate_limit.rs`), which throttles
//! by request *volume* — this instead asks a third-party verifier "was this
//! request driven by a human," blocking scripted credential-stuffing/
//! signup-bot traffic a volume limiter alone can't distinguish from a slow
//! human retrying a typo.
//!
//! **Provider abstraction.** Every popular CAPTCHA provider (Cloudflare
//! Turnstile, hCaptcha, Google reCAPTCHA v2/v3) exposes the *same*
//! "siteverify" wire shape: `POST` `secret` + `response` (+ optional
//! `remoteip`) form-encoded, get back JSON with a `success: bool` (plus
//! provider-specific extra fields this module ignores). So the actual HTTP
//! call ([`verify_with_provider`]) is entirely provider-agnostic; the only
//! provider-specific piece is the default siteverify URL
//! ([`default_verify_url`]). **Turnstile ships as the default and is the
//! only provider exercised by tests** (mocked — no real network, no real
//! secret); hCaptcha/reCAPTCHA select the same code path with a different
//! default URL, so "slot in later" is already true today rather than
//! aspirational.
//!
//! **Config** (`docs/REST_API.md` documents these):
//! - `UNIDB_CAPTCHA_PROVIDER` (default `turnstile`) — `turnstile` |
//!   `hcaptcha` | `recaptcha`. Only picks the *default* verify URL; an
//!   unrecognized value falls back to Turnstile's default (still
//!   overridable via `UNIDB_CAPTCHA_VERIFY_URL`).
//! - `UNIDB_CAPTCHA_VERIFY_URL` (optional) — override the siteverify
//!   endpoint. This is the seam tests point at a local mock server, exactly
//!   like `UNIDB_OAUTH_<PROVIDER>_TOKEN_URL`/`_USERINFO_URL` do for OAuth
//!   (`oauth.rs`'s module doc) — no real network needed to test this
//!   feature.
//! - `UNIDB_CAPTCHA_SECRET` (optional) — the provider's server-side secret
//!   key, env-sourced fallback. Resolved **vault-first**
//!   ([`CaptchaConfig::resolve_secret`]) exactly like OAuth's client secret
//!   (item 129, I3): a vault secret stored under [`CAPTCHA_SECRET_NAME`]
//!   ([`crate::authz::RoleStore::get_secret`]) wins when present, falling
//!   back to this env value.
//! - `UNIDB_CAPTCHA_PROTECT` (default **empty = disabled**) — comma-
//!   separated list of endpoints that require a verified CAPTCHA token:
//!   `signup`, `login`. Anything not listed — including everything, by
//!   default — behaves exactly as it did before this item shipped: no
//!   token required, back-compat preserved unconditionally.
//!
//! **Fail-closed, once enabled.** For a *protected* endpoint: no token, an
//! invalid/expired token, a provider that explicitly rejects verification,
//! and a misconfigured/unreachable verifier (secret unresolvable, network
//! error, malformed response) are **all** treated as "block the request" —
//! never a silent pass-through. [`CaptchaError::Failed`] deliberately
//! collapses "wrong token" and "verifier misconfigured" into one uniform
//! outward failure (`403 CAPTCHA_FAILED`, see `error.rs`), mirroring how
//! `POST /auth/login` already gives no oracle on *why* a login failed
//! (`handlers.rs::post_auth_login`) — a caller cannot distinguish "your
//! token was wrong" from "the operator misconfigured their CAPTCHA secret."
//! The one distinguished case is a **missing** (or empty-string) token on a
//! protected endpoint, which is a client-request-shape problem
//! ([`CaptchaError::TokenRequired`], `400 CAPTCHA_TOKEN_REQUIRED`), not a
//! verification outcome.
//!
//! **Never logged.** Neither the provider secret nor the client-supplied
//! token is ever `Debug`-printed or passed to a `tracing::*!` call — see
//! `dto.rs`'s manual `Debug` impls for `AuthLoginRequest`/
//! `AuthSignupRequest`, which redact `captcha_token` the same way
//! `password` already is.

use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;

use crate::server::engine_handle::EngineHandle;

/// Vault secret name for the CAPTCHA provider's server-side secret key —
/// mirrors `oauth::oauth_secret_name`'s convention, but unlike OAuth (one
/// secret per provider) there is one active CAPTCHA provider at a time, so
/// this is a single fixed name rather than a per-provider format string.
/// `pub` so both this module and an operator running `unidb-vault set`
/// know the exact literal.
pub const CAPTCHA_SECRET_NAME: &str = "captcha.secret";

const TURNSTILE_VERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
const HCAPTCHA_VERIFY_URL: &str = "https://hcaptcha.com/siteverify";
const RECAPTCHA_VERIFY_URL: &str = "https://www.google.com/recaptcha/api/siteverify";

/// The default siteverify URL for a recognized provider name. Turnstile is
/// both the default provider and the fallback for any unrecognized name —
/// `UNIDB_CAPTCHA_VERIFY_URL` always overrides this regardless.
fn default_verify_url(provider: &str) -> &'static str {
    match provider {
        "hcaptcha" => HCAPTCHA_VERIFY_URL,
        "recaptcha" => RECAPTCHA_VERIFY_URL,
        _ => TURNSTILE_VERIFY_URL,
    }
}

/// Outward failure of a CAPTCHA gate check — mapped to HTTP status/code in
/// `error.rs`'s `From<CaptchaError> for ApiError`.
#[derive(Debug)]
pub enum CaptchaError {
    /// The endpoint requires a token and none (or only an empty string) was
    /// supplied — a request-shape problem, distinguished from a
    /// verification failure (see the module doc). Maps to `400`.
    TokenRequired,
    /// Bad/expired token, the provider explicitly rejected verification, or
    /// the server itself could not complete verification (secret
    /// unresolvable, network/timeout, malformed provider response) — all
    /// collapsed into one uniform outward failure, fail-closed (see the
    /// module doc). Maps to `403`.
    Failed,
}

#[derive(Deserialize)]
struct SiteverifyResponse {
    success: bool,
}

/// Provider-agnostic CAPTCHA gate, held on [`crate::server::AppState`].
/// Disabled by default ([`CaptchaConfig::disabled`] — empty `protect` set) —
/// see the module doc's back-compat guarantee: with the default config,
/// [`CaptchaConfig::enforce`] is a no-op for every endpoint.
#[derive(Clone)]
pub struct CaptchaConfig {
    /// Raw `UNIDB_CAPTCHA_SECRET` value (may be empty — [`Self::resolve_secret`]
    /// checks the vault first, then falls back to this).
    env_secret: String,
    verify_url: String,
    /// Endpoint identifiers (`"login"`, `"signup"`) currently gated.
    protect: HashSet<String>,
    http: reqwest::Client,
}

impl Default for CaptchaConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

impl CaptchaConfig {
    /// No endpoints protected — [`Self::enforce`] is a no-op for every
    /// endpoint regardless of secret/verify_url. The safe, back-compat
    /// default (see the module doc).
    pub fn disabled() -> Self {
        Self {
            env_secret: String::new(),
            verify_url: TURNSTILE_VERIFY_URL.to_string(),
            protect: HashSet::new(),
            http: build_http_client(),
        }
    }

    /// Explicit construction — used by tests to point at a local mock
    /// siteverify server and pick which endpoints are protected, without
    /// touching process-global environment variables.
    pub fn new(
        env_secret: impl Into<String>,
        verify_url: impl Into<String>,
        protect: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            env_secret: env_secret.into(),
            verify_url: verify_url.into(),
            protect: protect.into_iter().map(Into::into).collect(),
            http: build_http_client(),
        }
    }

    /// `UNIDB_CAPTCHA_PROVIDER` (default `turnstile`) /
    /// `UNIDB_CAPTCHA_VERIFY_URL` / `UNIDB_CAPTCHA_SECRET` /
    /// `UNIDB_CAPTCHA_PROTECT` — see the module doc for each.
    pub fn from_env() -> Self {
        let provider = std::env::var("UNIDB_CAPTCHA_PROVIDER")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "turnstile".to_string());
        let verify_url = std::env::var("UNIDB_CAPTCHA_VERIFY_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_verify_url(&provider).to_string());
        let env_secret = std::env::var("UNIDB_CAPTCHA_SECRET").unwrap_or_default();
        let protect: HashSet<String> = std::env::var("UNIDB_CAPTCHA_PROTECT")
            .ok()
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            env_secret,
            verify_url,
            protect,
            http: build_http_client(),
        }
    }

    /// Whether `endpoint` (`"login"` / `"signup"`) currently requires a
    /// verified CAPTCHA token.
    pub fn protects(&self, endpoint: &str) -> bool {
        self.protect.contains(endpoint)
    }

    /// Whether any endpoint at all requires CAPTCHA — used only for a
    /// startup log line (`unidb-server.rs`), not on any request path.
    pub fn is_enabled(&self) -> bool {
        !self.protect.is_empty()
    }

    /// Resolve the effective CAPTCHA secret: the vault
    /// ([`CAPTCHA_SECRET_NAME`]) first, falling back to `env_secret` —
    /// identical order/semantics to
    /// [`crate::server::oauth::resolve_client_secret`]. `Err` (never a
    /// silent empty/placeholder secret) when the vault lookup itself fails
    /// (disabled vault holding a stored secret, wrong/rotated key, tampered
    /// blob) or the secret is in neither place.
    async fn resolve_secret(&self, engine: &EngineHandle) -> Result<String, String> {
        match engine.get_secret(CAPTCHA_SECRET_NAME.to_string()).await {
            Ok(Some(secret)) => Ok(secret),
            Ok(None) => {
                if self.env_secret.is_empty() {
                    Err(format!(
                        "no CAPTCHA secret configured — set it via \
                         `unidb-vault set {CAPTCHA_SECRET_NAME}` or UNIDB_CAPTCHA_SECRET"
                    ))
                } else {
                    Ok(self.env_secret.clone())
                }
            }
            Err(e) => Err(format!("vault error resolving CAPTCHA secret: {e}")),
        }
    }

    /// The gate every protected handler calls, in order: no-op when
    /// `endpoint` isn't protected -> require a non-empty `token` -> resolve
    /// the secret -> call the provider -> `Ok(())` only on an explicit
    /// `{"success": true}`. See the module doc for the fail-closed
    /// contract. `remote_ip` is optional context passed through to the
    /// provider's `remoteip` field (informational on their end; verification
    /// never depends on it here).
    pub async fn enforce(
        &self,
        engine: &EngineHandle,
        endpoint: &str,
        token: Option<&str>,
        remote_ip: Option<&str>,
    ) -> Result<(), CaptchaError> {
        if !self.protects(endpoint) {
            return Ok(());
        }
        let token = token
            .filter(|t| !t.is_empty())
            .ok_or(CaptchaError::TokenRequired)?;
        let secret = self
            .resolve_secret(engine)
            .await
            .map_err(|_| CaptchaError::Failed)?;
        match verify_with_provider(&self.http, &self.verify_url, &secret, token, remote_ip).await {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => Err(CaptchaError::Failed),
        }
    }
}

/// Outbound HTTP client for the siteverify call, with a bounded timeout —
/// same shape/rationale as `oauth::build_http_client` (a slow/unresponsive
/// verifier must fail the request, not hang it); never `unwrap`/`expect`.
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// The provider-agnostic siteverify call (see the module doc). Server-error
/// status and network/parse failures are folded into `Err` — the caller
/// ([`CaptchaConfig::enforce`]) treats `Err` the same as an explicit
/// `success: false`, fail-closed.
async fn verify_with_provider(
    client: &reqwest::Client,
    verify_url: &str,
    secret: &str,
    token: &str,
    remote_ip: Option<&str>,
) -> Result<bool, String> {
    let mut form: Vec<(&str, &str)> = vec![("secret", secret), ("response", token)];
    if let Some(ip) = remote_ip {
        form.push(("remoteip", ip));
    }
    let resp = client
        .post(verify_url)
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("captcha verify request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "captcha verify endpoint returned {}",
            resp.status()
        ));
    }
    let body: SiteverifyResponse = resp
        .json()
        .await
        .map_err(|e| format!("malformed captcha verify response: {e}"))?;
    Ok(body.success)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default_protects_nothing() {
        let cfg = CaptchaConfig::disabled();
        assert!(!cfg.protects("login"));
        assert!(!cfg.protects("signup"));
        assert!(!cfg.is_enabled());
    }

    #[test]
    fn explicit_protect_list_gates_only_named_endpoints() {
        let cfg = CaptchaConfig::new("secret", "http://example.invalid", ["signup", "login"]);
        assert!(cfg.protects("login"));
        assert!(cfg.protects("signup"));
        assert!(!cfg.protects("refresh"));
        assert!(cfg.is_enabled());
    }

    #[test]
    fn default_verify_url_picks_turnstile_by_default_and_for_unknown_names() {
        assert_eq!(default_verify_url("turnstile"), TURNSTILE_VERIFY_URL);
        assert_eq!(
            default_verify_url("some-unrecognized-provider"),
            TURNSTILE_VERIFY_URL
        );
        assert_eq!(default_verify_url("hcaptcha"), HCAPTCHA_VERIFY_URL);
        assert_eq!(default_verify_url("recaptcha"), RECAPTCHA_VERIFY_URL);
    }
}
