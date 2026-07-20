//! Verify-only, stateless JWT auth (M5.c): the server validates bearer
//! tokens signed with a shared secret (HS256, via `jsonwebtoken`'s
//! `aws_lc_rs` crypto backend) supplied at startup — there is no login
//! endpoint, no user/credential database, and no session state. Whatever
//! issues the tokens (an external auth service, a shared secret
//! distributed out-of-band) is entirely outside this project's scope; the
//! embedded `Engine` never sees a token at all, verification happens
//! purely at this HTTP-layer boundary.
//!
//! Applied as a `tower::Layer` (`axum::middleware::from_fn_with_state`)
//! wrapping every data-plane route in `router.rs`. **Deliberately excluded
//! from `GET /metrics`**: Prometheus scrapers don't carry app-level bearer
//! tokens, and the operational expectation is that `/metrics` gets
//! firewalled at the network layer in production — the same "no TLS
//! termination, assume a reverse proxy" assumption already stated for the
//! rest of this server, not an oversight.

use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

/// Loaded once at startup from `UNIDB_JWT_SECRET` (see `src/bin/
/// unidb-server.rs`). HS256 only in v1 — RS256/ES256 (asymmetric,
/// verify-with-public-key) is a straightforward `jsonwebtoken`-native
/// extension but not required to satisfy the locked "verify-only, one
/// shared secret" scope.
///
/// When `UNIDB_DEV_LOGIN=1` is set at startup, `encoding_key` is populated
/// and `POST /auth/login` issues short-lived tokens.  This is deliberately a
/// **dev/demo-only path** — it does not reopen Milestone-18's "verify-only"
/// decision for production; the production posture (tokens come from an
/// external IdP) is unchanged when the flag is absent.
#[derive(Clone)]
pub struct JwtConfig {
    decoding_key: DecodingKey,
    validation: Validation,
    /// Non-None only when `UNIDB_DEV_LOGIN=1` — used by `POST /auth/login`.
    pub encoding_key: Option<EncodingKey>,
}

impl JwtConfig {
    /// Verify-only config (production default).
    pub fn new(secret: &str) -> Self {
        Self {
            decoding_key: DecodingKey::from_secret(secret.as_bytes()),
            validation: Validation::new(Algorithm::HS256),
            encoding_key: None,
        }
    }

    /// Verify + issue config (`UNIDB_DEV_LOGIN=1`).  Same secret is used for
    /// both — the token issued here will pass the existing `require_jwt`
    /// middleware without change.
    pub fn with_dev_login(secret: &str) -> Self {
        Self {
            decoding_key: DecodingKey::from_secret(secret.as_bytes()),
            validation: Validation::new(Algorithm::HS256),
            encoding_key: Some(EncodingKey::from_secret(secret.as_bytes())),
        }
    }

    /// Issue a short-lived HS256 JWT for `username`.
    ///
    /// Returns `Err` when `encoding_key` is `None` (dev-login flag is off).
    pub fn issue_token(&self, username: &str) -> Result<String, jsonwebtoken::errors::Error> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let key = self.encoding_key.as_ref().expect("issue_token called without encoding_key");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let claims = serde_json::json!({
            "sub": username,
            "iat": now,
            "exp": now + 3600, // 1 hour
        });
        encode(&Header::default(), &claims, key)
    }
}

/// Deliberately permissive: no required custom claims beyond whatever
/// `jsonwebtoken`'s `Validation` already checks by default (`exp`, if
/// present, is validated; `nbf`/`aud`/`iss` are opt-in and not required
/// here). There is no role/scope claim distinction in v1 — any validly
/// signed, unexpired token grants access to every data-plane route alike
/// (see the known-limitations note in `PROGRESS.md`/`MEMORY.md`).
#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    /// The subject = the unidb username (P6.e). Absent ⇒ an anonymous but
    /// authenticated client, treated as the implicit superuser (backward
    /// compatible with pre-P6.e tokens that carry no `sub`).
    sub: Option<String>,
    #[serde(flatten)]
    _extra: std::collections::HashMap<String, serde_json::Value>,
}

/// The authenticated user carried through request extensions (P6.e). `None`
/// (no `sub` claim) is the implicit superuser.
#[derive(Clone, Debug)]
pub struct CurrentUser(pub Option<String>);

#[derive(Serialize)]
struct AuthErrorBody {
    error: String,
    code: &'static str,
}

fn unauthorized(msg: impl Into<String>) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(AuthErrorBody {
            error: msg.into(),
            code: "UNAUTHORIZED",
        }),
    )
        .into_response()
}

/// `axum::middleware::from_fn_with_state`-compatible middleware: extracts
/// `Authorization: Bearer <token>`, verifies it, and only then forwards
/// the request — any failure (missing header, malformed header, bad
/// signature, expired token) short-circuits with 401 and never reaches
/// the handler.
pub async fn require_jwt(
    axum::extract::State(config): axum::extract::State<JwtConfig>,
    request: Request,
    next: Next,
) -> Response {
    let Some(header_value) = request.headers().get(header::AUTHORIZATION) else {
        return unauthorized("missing Authorization header");
    };
    let Ok(header_str) = header_value.to_str() else {
        return unauthorized("Authorization header is not valid UTF-8");
    };
    let Some(token) = header_str.strip_prefix("Bearer ") else {
        return unauthorized("Authorization header must be a Bearer token");
    };

    let start = std::time::Instant::now();
    let result = decode::<Claims>(token, &config.decoding_key, &config.validation);
    metrics::histogram!("unidb_jwt_verify_seconds").record(start.elapsed().as_secs_f64());

    match result {
        Ok(data) => {
            // Carry the authenticated username to handlers for per-user
            // privilege checks (P6.e).
            let mut request = request;
            request
                .extensions_mut()
                .insert(CurrentUser(data.claims.sub));
            next.run(request).await
        }
        Err(e) => unauthorized(format!("invalid token: {e}")),
    }
}
