//! Request correlation ids (item 22, L2).
//!
//! Every HTTP request is stamped with a `request_id` the moment it enters the
//! server, *before* auth, so even a rejected (401/403) request is traceable.
//! The id travels three ways, each serving a different consumer:
//!
//! 1. A [`tracing`] span (`http_request`) entered for the whole request →
//!    every app-log line the async side emits carries `request_id`.
//! 2. A tokio **task-local** ([`REQUEST_ID`]) → readable by the async
//!    `EngineHandle` wrappers, which copy it onto the blocking pool thread that
//!    runs the synchronous engine call (see `engine_handle.rs`), where it feeds
//!    the engine-core thread-local ([`crate::observability`]). That is how the
//!    slow-query log and `audit.log` — written deep in the engine — get the id.
//! 3. A response header (`x-request-id`) → the client (and the studio Logs tab,
//!    L4) can show/join on it without parsing the body.
//!
//! The id itself is process-local and cheap: a per-process random seed mixed
//! with a monotonic counter, hex-encoded. It needs to be unique within one
//! server's log retention window and greppable — not globally unique — so no
//! UUID dependency is pulled in (this is a single-node server; `CLAUDE.md` §1).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};
use tracing::Instrument;

tokio::task_local! {
    /// The current request's `request_id`, scoped for the lifetime of the
    /// request future so nested async engine calls can read it.
    pub static REQUEST_ID: String;
}

/// The current task's `request_id`, or `None` outside a request scope (e.g. the
/// background reaper or SSE poll loop).
pub fn current_request_id() -> Option<String> {
    REQUEST_ID.try_with(|id| id.clone()).ok()
}

/// Process-lifetime seed so ids from different server processes don't collide
/// in a shared log sink; `OnceLock`-free since we can seed it at first use.
fn seed() -> u64 {
    static SEED: AtomicU64 = AtomicU64::new(0);
    let existing = SEED.load(Ordering::Relaxed);
    if existing != 0 {
        return existing;
    }
    let s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1; // never 0 (0 is the "unseeded" sentinel)
             // First writer wins; a race just means two callers computed similar seeds.
    SEED.store(s, Ordering::Relaxed);
    s
}

/// A short, greppable, per-request id (e.g. `req-1a2b3c4d5e6f7a8b`).
pub fn new_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Mix the seed with the counter so ids are unpredictable yet unique
    // in-process; wrapping_mul by an odd constant scrambles adjacent counters.
    let mixed = seed() ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("req-{mixed:016x}")
}

/// Top-level middleware (applied outside auth) that assigns `request_id`,
/// scopes it as a task-local, enters an `http_request` span, and echoes it back
/// as `x-request-id`.
pub async fn assign_request_id(mut request: Request, next: Next) -> Response {
    let request_id = new_request_id();
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    // Make it available to handlers/extractors as well.
    request
        .extensions_mut()
        .insert(RequestId(request_id.clone()));

    let span = tracing::info_span!(
        "http_request",
        request_id = %request_id,
        method = %method,
        path = %path,
    );

    let rid_for_scope = request_id.clone();
    let mut response = REQUEST_ID
        .scope(rid_for_scope, next.run(request).instrument(span))
        .await;

    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// The request id carried in request extensions for handlers that want it.
#[derive(Clone, Debug)]
pub struct RequestId(pub String);
