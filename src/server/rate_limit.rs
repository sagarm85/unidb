//! In-memory brute-force protection for the password-auth mutation routes —
//! `POST /auth/login`, `/auth/signup`, `/auth/refresh` (item 121, Workstream
//! I, I1 — P0, ships with A). These are the classic credential-stuffing /
//! brute-force targets: unlike every other route in `router.rs` they are
//! reachable with **no** bearer token at all, so nothing upstream of the
//! handler stops a client from hammering them.
//!
//! **No external rate-limit crate.** This is the HTTP boundary handling at
//! most a few requests per second, not a hot path — a hand-rolled
//! fixed-window counter behind `Arc<Mutex<HashMap<..>>>` is the right amount
//! of machinery (CLAUDE.md's coding conventions favor the simplest correct
//! primitive; pulling in `governor`/`tower-governor` for this would be
//! exactly the "dependency-heavy framework" item I1 explicitly says not to
//! add).
//!
//! **Algorithm — fixed window per key.** Each key gets a counter and a
//! window-start [`Instant`]. Every attempt increments the counter; once the
//! window has fully elapsed since `window_start` the bucket resets. The
//! `(max_attempts + 1)`th attempt inside one window is rejected with `429`.
//! This is deliberately simpler than a sliding-window log or token bucket:
//! brute-force protection needs "no more than N attempts per window," not
//! smooth traffic shaping, and a fixed window is trivial to reason about and
//! to test deterministically.
//!
//! **Key.** `client_ip` (the TCP peer address) is the **required**
//! component — see [`client_ip`]'s doc comment for why this reads
//! `ConnectInfo`, not `X-Forwarded-For`. Folded into the same key: the
//! request path (`/auth/login` vs `/auth/signup` vs `/auth/refresh` each get
//! independent buckets) and, when the JSON body carries a `username` field
//! (login/signup), the username itself — so two different accounts hammering
//! from behind the same NAT/reverse-proxy IP don't share one bucket. This is
//! the optional half of the spec ("optionally also key by username... IP is
//! the MUST") — `/auth/refresh` has no username to peek at pre-verification,
//! so its key is IP+path only.
//!
//! **Time source.** [`std::time::Instant`], never wall-clock — immune to
//! clock adjustment/NTP step, matching the "no wall-clock for windows" rule.
//!
//! **Pruning.** Every [`AuthRateLimiter::check`] call opportunistically
//! drops every *other* bucket whose window has fully elapsed, so memory
//! never grows unbounded — no background sweep task needed for what is, at
//! steady state, a small map (one entry per currently-hammering client).
//!
//! **Config** (`docs/REST_API.md` documents these):
//! - `UNIDB_AUTH_RATE_LIMIT` — max attempts per window. Default `10`. `0`
//!   disables rate limiting entirely (the middleware becomes a no-op
//!   pass-through — chosen over a separate on/off flag so "unset = sane
//!   default" and "explicitly 0 = off" are the only two knobs needed).
//! - `UNIDB_AUTH_RATE_WINDOW_SECS` — window length in seconds. Default `60`.
//!
//! **Response.** `429 Too Many Requests`, JSON body
//! `{ "error": "...", "code": "RATE_LIMITED" }`, plus a `Retry-After` header
//! (seconds until the window resets, rounded up to at least 1).

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

/// One key's attempt counter for the current window.
struct Bucket {
    count: u32,
    window_start: Instant,
}

/// Shared, cloneable rate-limiter state — installed as `axum::middleware::
/// from_fn_with_state`'s state on the auth-mutation sub-router only (see
/// `router.rs`). Cheap to clone (one `Arc`).
#[derive(Clone)]
pub struct AuthRateLimiter {
    inner: Arc<Mutex<HashMap<String, Bucket>>>,
    max_attempts: u32,
    window: Duration,
}

impl AuthRateLimiter {
    /// `max_attempts == 0` disables limiting outright (see module doc).
    pub fn new(max_attempts: u32, window: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            max_attempts,
            window,
        }
    }

    /// Reads `UNIDB_AUTH_RATE_LIMIT` (default 10) and
    /// `UNIDB_AUTH_RATE_WINDOW_SECS` (default 60). A window of `0` seconds
    /// would make every attempt immediately "past the window" and reset on
    /// every request (i.e. no limiting at all) — clamped to at least 1s so a
    /// misconfigured `0` doesn't silently disable protection while
    /// `max_attempts` stays nonzero.
    pub fn from_env() -> Self {
        let max_attempts: u32 = std::env::var("UNIDB_AUTH_RATE_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        let window_secs: u64 = std::env::var("UNIDB_AUTH_RATE_WINDOW_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        Self::new(max_attempts, Duration::from_secs(window_secs.max(1)))
    }

    /// `UNIDB_AUTH_RATE_LIMIT=0` — the documented "off" switch.
    pub fn is_disabled(&self) -> bool {
        self.max_attempts == 0
    }

    /// Record one attempt for `key`. `Ok(())` = allowed; `Err(retry_after)` =
    /// over the limit, with how long until the window resets.
    fn check(&self, key: &str) -> Result<(), Duration> {
        let now = Instant::now();
        // A poisoned mutex (a prior panic while holding the lock) must not
        // take rate limiting itself down — recover the map rather than
        // propagate the poison (no `unwrap()` on the lock result).
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // Opportunistic prune (module doc): drop every *other* expired
        // bucket so the map never grows unbounded without a background task.
        let window = self.window;
        map.retain(|k, b| k == key || now.duration_since(b.window_start) < window);

        let bucket = map.entry(key.to_string()).or_insert_with(|| Bucket {
            count: 0,
            window_start: now,
        });
        if now.duration_since(bucket.window_start) >= self.window {
            bucket.window_start = now;
            bucket.count = 0;
        }
        bucket.count += 1;
        if bucket.count > self.max_attempts {
            let elapsed = now.duration_since(bucket.window_start);
            Err(self.window.saturating_sub(elapsed))
        } else {
            Ok(())
        }
    }
}

#[derive(Serialize)]
struct RateLimitedBody {
    error: String,
    code: &'static str,
}

fn rate_limited_response(retry_after: Duration) -> Response {
    // Round up to at least 1s: a `Retry-After: 0` is a contradiction (the
    // client would just retry immediately into the same still-open window).
    let secs = retry_after.as_secs().max(1);
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(RateLimitedBody {
            error: "too many auth attempts — slow down and try again later".to_string(),
            code: "RATE_LIMITED",
        }),
    )
        .into_response();
    if let Ok(value) = header::HeaderValue::from_str(&secs.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// Cap on how much of the request body this middleware buffers to peek at a
/// `username` field (see [`peek_username`]). Auth JSON bodies are a couple of
/// short strings; 1 MiB is already generous headroom, and a request that
/// doesn't fit is already a malformed/abusive login attempt the handler
/// would have rejected as bad JSON anyway.
const MAX_PEEK_BODY_BYTES: usize = 1024 * 1024;

/// Buffer the request body, try to pull a `"username"` string field out of
/// it as JSON, and return a fresh body carrying the same bytes so the
/// downstream handler's own `Json<...>` extractor still sees the original
/// payload. Reading fails (body too large, connection error) ⇒ the
/// discriminator is simply absent (the handler itself will fail to parse the
/// same body right after and return its own error) — never blocks the
/// request here on a body-read problem this middleware isn't responsible
/// for reporting.
async fn peek_username(body: Body) -> (Body, Option<String>) {
    match axum::body::to_bytes(body, MAX_PEEK_BODY_BYTES).await {
        Ok(bytes) => {
            let username = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| {
                    v.get("username")
                        .and_then(|u| u.as_str().map(str::to_string))
                });
            (Body::from(bytes), username)
        }
        Err(_) => (Body::empty(), None),
    }
}

/// The client IP this request is keyed on: the TCP peer address
/// (`ConnectInfo<SocketAddr>`, wired via `into_make_service_with_connect_info`
/// in both `unidb-server.rs` and the integration-test harness).
///
/// **Deliberately not `X-Forwarded-For`.** That header is client-supplied and
/// trivially spoofed by anyone who can reach this server directly — trusting
/// it blindly would let an attacker paint every brute-force attempt with a
/// distinct fake IP and defeat the whole point of this limiter. Honoring it
/// safely requires a trusted-proxy configuration (a known set of upstream
/// hops whose *own* `X-Forwarded-For` additions can be trusted, stripping
/// anything the client itself supplied) — no such config surface exists
/// anywhere else in this server yet (see `auth.rs`'s module doc: "no TLS
/// termination, assume a reverse proxy" is already the documented deployment
/// assumption, but it is a network-topology assumption, not a parsed,
/// trusted-hop-count config this code could act on). So: peer address only,
/// for now — a real reverse-proxy deployment terminates TLS and forwards
/// from one known upstream IP, and rate limiting that one IP still protects
/// the origin from being hammered directly. Adding a trusted-proxy
/// `X-Forwarded-For` mode is a natural follow-up once such a config exists.
fn client_ip(addr: SocketAddr) -> std::net::IpAddr {
    addr.ip()
}

/// `axum::middleware::from_fn_with_state`-compatible middleware, layered
/// (see `router.rs`) over exactly `POST /auth/login`, `/auth/signup`,
/// `/auth/refresh` — never `/sql`, `/metrics`, `/.well-known/jwks.json`,
/// `/auth/meta`, or any read/data route. A rejected (401) attempt counts
/// toward the limit exactly like an accepted one: a brute-forcer's requests
/// are 401s, and those must count, so counting happens here, before the
/// handler runs, on every request regardless of outcome.
pub async fn rate_limit_auth(
    State(limiter): State<AuthRateLimiter>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if limiter.is_disabled() {
        return next.run(request).await;
    }

    let path = request.uri().path().to_string();
    let (parts, body) = request.into_parts();
    let (body, username) = peek_username(body).await;
    let request = Request::from_parts(parts, body);

    let ip = client_ip(addr);
    let key = match username {
        Some(user) => format!("{ip}|{path}|{user}"),
        None => format!("{ip}|{path}"),
    };

    match limiter.check(&key) {
        Ok(()) => next.run(request).await,
        Err(retry_after) => rate_limited_response(retry_after),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max_attempts_then_rejects() {
        let limiter = AuthRateLimiter::new(3, Duration::from_secs(60));
        assert!(limiter.check("k").is_ok());
        assert!(limiter.check("k").is_ok());
        assert!(limiter.check("k").is_ok());
        // 4th attempt in the same window is over the limit.
        assert!(limiter.check("k").is_err());
    }

    #[test]
    fn different_keys_are_independent() {
        let limiter = AuthRateLimiter::new(1, Duration::from_secs(60));
        assert!(limiter.check("a").is_ok());
        assert!(limiter.check("a").is_err());
        // A different key has its own untouched bucket.
        assert!(limiter.check("b").is_ok());
    }

    #[test]
    fn zero_max_attempts_means_disabled() {
        let limiter = AuthRateLimiter::new(0, Duration::from_secs(60));
        assert!(limiter.is_disabled());
    }

    #[test]
    fn nonzero_max_attempts_means_enabled() {
        let limiter = AuthRateLimiter::new(5, Duration::from_secs(60));
        assert!(!limiter.is_disabled());
    }

    #[test]
    fn window_resets_after_it_elapses() {
        let limiter = AuthRateLimiter::new(1, Duration::from_millis(30));
        assert!(limiter.check("k").is_ok());
        assert!(limiter.check("k").is_err());
        std::thread::sleep(Duration::from_millis(60));
        // The window has fully elapsed — the bucket resets and a fresh
        // attempt is allowed again.
        assert!(limiter.check("k").is_ok());
    }

    #[test]
    fn retry_after_does_not_exceed_the_window() {
        let limiter = AuthRateLimiter::new(1, Duration::from_secs(60));
        assert!(limiter.check("k").is_ok());
        let Err(retry_after) = limiter.check("k") else {
            panic!("expected the second attempt to be rejected");
        };
        assert!(retry_after <= Duration::from_secs(60));
    }
}
