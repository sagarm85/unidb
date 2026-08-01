//! Outbound webhook delivery worker (item 141, backlog doc's locked v1
//! design): background, best-effort, **at-least-once** HTTP delivery of
//! committed row-change events to superuser-registered endpoints — the
//! Supabase "Database Webhooks" feature, built entirely on top of the
//! already-durable M4/item-29 event queue.
//!
//! **No storage-engine change.** This worker is strictly *downstream* of
//! commit: it reads only already-committed events via the existing
//! [`crate::Engine::poll_events`] durable-consumer path (the exact mechanism
//! `server/sse.rs` uses for `GET /events/subscribe`) and writes only its own
//! consumer offset via [`crate::Engine::ack_events`]. No WAL/heap/MVCC
//! interaction beyond that.
//!
//! ## Lifecycle (mirrors `server/mod.rs`'s `spawn_reaper`, not `autovacuum`/
//! `stats_ticker`)
//!
//! `autovacuum.rs`/`stats_ticker.rs` spawn a plain `std::thread` because
//! their work is fully synchronous. This worker needs `reqwest`, which is
//! async-only in this crate's enabled feature set (no `blocking` feature),
//! so it is spawned as a `tokio::spawn` task instead — same shape as
//! `spawn_reaper`: it holds only a [`Weak<EngineHandle>`], so it never keeps
//! the server alive, and the next tick's `upgrade()` failing is the exit
//! signal (no explicit shutdown channel needed). [`spawn_webhook_worker`]
//! must be called from inside a tokio runtime (from [`crate::server::
//! AppState::with_config`], alongside `spawn_reaper`).
//!
//! ## Delivery
//!
//! One durable consumer (name: [`CONSUMER_NAME`]) polls `__events__` on the
//! same commit-condvar push / idle-fallback rhythm `sse.rs` uses
//! ([`crate::server::engine_handle::EngineHandle::wait_event_commit`]). Each
//! polled batch is matched against every enabled [`crate::authz::WebhookDef`]
//! by `table_pattern` (exact or `*`, [`crate::authz::webhook_table_matches`])
//! and `events` together; a match is POSTed as the native CDC envelope JSON
//! (the same shape `GET /events/subscribe?format=native` emits —
//! [`crate::server::event_format::format_event`]) with an
//! `X-Unidb-Signature: sha256=<hex HMAC-SHA256>` header when the webhook has
//! a signing secret (vault-first, [`resolve_secret`]).
//!
//! Every delivery in one poll tick runs on its own `tokio::spawn` task, so a
//! dead endpoint's bounded retry/backoff never delays a healthy webhook's
//! delivery, and one table's webhook never blocks another's. Each delivery
//! gets bounded exponential-backoff retry ([`MAX_ATTEMPTS`], capped at 5 per
//! the backlog spec) with a per-attempt timeout
//! ([`ATTEMPT_TIMEOUT`]); on final exhaustion it logs a warning, increments
//! `unidb_webhook_delivery_failures_total`, and is simply dropped — **the
//! consumer offset always advances past the whole polled batch once every
//! delivery task in that tick has finished (succeeded or exhausted)**, so a
//! permanently dead endpoint can never wedge the stream for itself or for
//! any other registered webhook.
//!
//! This is why delivery is **at-least-once, not exactly-once**: a crash
//! between a delivery task's successful POST and the batch's ack can
//! redeliver that event to that endpoint on restart (the standard M4/Kafka-
//! style durable-consumer contract already documented for `poll_events`/
//! `ack_events`). A receiver that needs idempotency should dedupe on the CDC
//! envelope's `(table, xid, seq)` triple.
//!
//! **Not the same thing as `unidb-dispatch` (item 20/Epic E2).** That crate's
//! `WebhookSink` is a separate, app-embedded Rust library for a *custom app
//! process* to wire up its own webhook fan-out (`Filter`/`Sink`, its own
//! dead-letter table). This module is the built-in feature: REST-admin
//! registration (`POST`/`GET`/`DELETE /webhooks`) and delivery running
//! inside `unidb-server` itself — no separate app process, no shared code or
//! state with `unidb-dispatch`.

use std::sync::Weak;
use std::time::Duration;

use crate::authz::{webhook_table_matches, WebhookDef, WebhookEvent};
use crate::queue::Event;
use crate::server::engine_handle::EngineHandle;

/// Durable consumer name the delivery worker registers under in
/// `__consumers__` — double-underscore-namespaced like `__events__`/
/// `__consumers__` themselves, so it can never collide with an
/// operator-chosen consumer name (`POST /events/ack` accepts any string).
const CONSUMER_NAME: &str = "__webhooks__";

/// Bound on delivery attempts per event per webhook (backlog spec: "≤5
/// attempts").
const MAX_ATTEMPTS: u32 = 5;

/// Exponential backoff base between attempts: 20ms, 40ms, 80ms, 160ms after
/// attempts 1..4 (attempt 5 is the last — no further wait). Kept small
/// deliberately: this is a background retry loop insulated from the
/// request/commit path by construction (see the module doc), not a
/// user-facing wait, so there is no reason to make it slow.
const BACKOFF_BASE: Duration = Duration::from_millis(20);

/// Per-attempt HTTP timeout — a slow/hanging endpoint fails its attempt
/// instead of hanging a delivery task (and therefore the batch it belongs
/// to) indefinitely.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

/// Max events fetched per poll tick — mirrors `sse.rs`'s `default_limit`.
const POLL_LIMIT: usize = 200;

/// Idle-fallback poll interval — mirrors `sse.rs`'s `default_interval_ms`.
const FALLBACK_INTERVAL: Duration = Duration::from_millis(500);

/// Outbound HTTP client for webhook deliveries, with a bounded default
/// timeout (overridden per-request when a delivery needs a specific
/// deadline). Never `unwrap`/`expect` (CLAUDE.md §4): falls back to an
/// un-configured default client in the vanishingly unlikely case the
/// builder itself fails, mirroring `oauth::build_http_client`.
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(ATTEMPT_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Hand-rolled HMAC-SHA256 (RFC 2104), built directly on the crate's
/// existing `sha2` dependency (`Sha256`, already vendored — see
/// `authz::sha256_hex`). **Not** routed through the `hmac` crate this crate
/// also vendors (used by `authz::hotp` for HMAC-SHA1): that `hmac = "0.13"`
/// requires `digest` 0.11-shaped hash types, while the `sha2 = "0.10"`
/// vendored here only implements `digest` 0.10's traits — two
/// semver-incompatible copies of `digest` end up in the dependency graph
/// (`cargo tree` confirms both `0.10.7` and `0.11.3` present, pulled in by
/// unrelated deps), and `Hmac<Sha256>` fails to type-check across that
/// split. HMAC's construction is exactly this RFC's double-hash — a small,
/// fixed, testable primitive (see the `hmac_sha256_matches_rfc4231_test_
/// vector_1` test below), so hand-rolling it here avoids pulling in yet
/// another pinned crate version just for one call site.
fn hmac_sha256(secret: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK_SIZE: usize = 64;
    let mut key_block = [0u8; BLOCK_SIZE];
    if secret.len() > BLOCK_SIZE {
        let hashed = Sha256::digest(secret);
        key_block[..hashed.len()].copy_from_slice(&hashed);
    } else {
        key_block[..secret.len()].copy_from_slice(secret);
    }
    let mut ipad = [0x36u8; BLOCK_SIZE];
    let mut opad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let inner = {
        let mut hasher = Sha256::new();
        hasher.update(ipad);
        hasher.update(message);
        hasher.finalize()
    };
    let outer = {
        let mut hasher = Sha256::new();
        hasher.update(opad);
        hasher.update(inner);
        hasher.finalize()
    };
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer);
    out
}

/// `X-Unidb-Signature` header value: `sha256=<hex HMAC-SHA256(secret, body)>`
/// — same hex-encoding convention as every other token/hash in this crate
/// (see `authz::sha256_hex` and friends). Always `Some` — HMAC accepts any
/// key length, so this never fails. `pub` (not just `pub(crate)`) so
/// `tests/item141_webhooks.rs` (a separate integration-test crate) can
/// independently compute the expected header for a captured delivery and
/// assert byte-for-byte equality against what was actually sent over the
/// wire — the cryptographic correctness of the underlying HMAC itself is
/// covered separately by this module's own `hmac_sha256_matches_rfc4231_
/// test_vector_1` unit test.
pub fn sign(secret: &str, body: &[u8]) -> Option<String> {
    let digest = hmac_sha256(secret.as_bytes(), body);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!("sha256={hex}"))
}

/// Whether `def` should receive `event`: enabled, `table_pattern` matches,
/// and the event's op is in `def.events`. A captured event whose `op` string
/// doesn't parse as a [`WebhookEvent`] (shouldn't happen — `event_format.rs`
/// and this module share the exact same three-op vocabulary — but fails
/// closed rather than panicking if it ever did) never matches.
fn webhook_matches(def: &WebhookDef, event: &Event) -> bool {
    def.enabled
        && webhook_table_matches(&def.table_pattern, &event.table_name)
        && WebhookEvent::parse(&event.op).is_some_and(|op| def.events.contains(&op))
}

/// Resolve the effective signing secret for `def`: vault
/// (`webhook.<id>.secret`) first, falling back to the plaintext captured at
/// registration time — the exact vault-then-fallback order [`crate::server::
/// oauth::resolve_client_secret`] uses for OAuth client secrets. Unlike that
/// resolver, a vault error here does not abort delivery — a webhook with a
/// broken vault entry falls back to its registration-time secret (if any)
/// rather than silently dropping every event, since (unlike OAuth token
/// exchange) there is no caller waiting on an HTTP response to carry the
/// error back to.
async fn resolve_secret(engine: &EngineHandle, def: &WebhookDef) -> Option<String> {
    let vault_name = format!("webhook.{}.secret", def.id);
    match engine.get_secret(vault_name).await {
        Ok(Some(secret)) => Some(secret),
        Ok(None) => def.signing_secret.as_ref().map(|s| s.expose().to_string()),
        Err(e) => {
            tracing::warn!(
                webhook = %def.id,
                error = %e,
                "vault lookup failed for webhook signing secret; falling back to the registration-time secret"
            );
            def.signing_secret.as_ref().map(|s| s.expose().to_string())
        }
    }
}

/// Deliver `body` to one webhook with bounded exponential-backoff retry
/// (see the module doc). Never panics — any failure (transport error,
/// non-2xx status, exhausted retries) is isolated to this one task and only
/// ever produces a log line + a metric increment, never a propagated error:
/// a delivery is not allowed to affect any other delivery, the poll loop, or
/// the engine.
async fn deliver_with_retry(
    client: &reqwest::Client,
    def: &WebhookDef,
    body: String,
    signature: Option<String>,
) {
    let mut delay = BACKOFF_BASE;
    for attempt in 1..=MAX_ATTEMPTS {
        let mut req = client
            .post(&def.target_url)
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        if let Some(sig) = &signature {
            req = req.header("X-Unidb-Signature", sig.as_str());
        }
        for (name, value) in &def.headers {
            req = req.header(name.as_str(), value.as_str());
        }
        match req.body(body.clone()).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!(webhook = %def.id, attempt, "webhook delivery succeeded");
                return;
            }
            Ok(resp) => {
                tracing::debug!(
                    webhook = %def.id,
                    status = %resp.status(),
                    attempt,
                    "webhook delivery attempt failed (non-2xx response)"
                );
            }
            Err(err) => {
                tracing::debug!(
                    webhook = %def.id,
                    error = %err,
                    attempt,
                    "webhook delivery attempt failed (transport error)"
                );
            }
        }
        if attempt < MAX_ATTEMPTS {
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
    }
    tracing::warn!(
        webhook = %def.id,
        target = %def.target_url,
        attempts = MAX_ATTEMPTS,
        "webhook delivery exhausted retries; advancing past it without blocking other webhooks"
    );
    metrics::counter!("unidb_webhook_delivery_failures_total").increment(1);
}

/// Spawn the background delivery worker. Always runs — no config gate — but
/// **only ever touches `__events__`/`__consumers__` while at least one
/// webhook is registered** ([`EngineHandle::list_webhooks`] checked first,
/// every tick, before any `poll_events`/`ack_events` call): with zero
/// webhooks registered this is a pure in-memory wait with no consumer row
/// ever created, so `Engine::vacuum_events`'s "reclaim once every
/// *registered* consumer has acked" contract (R3) is byte-for-byte
/// unaffected by a server that happens to have this worker running but no
/// webhooks configured — the common case for every test/deployment that
/// doesn't use this feature. Once a webhook IS registered, the durable
/// `__webhooks__` consumer starts advancing and legitimately participates in
/// that horizon from then on, exactly like any other named consumer — a
/// webhook whose delivery is still catching up must not have its
/// not-yet-delivered events vacuumed out from under it.
///
/// Must be called from inside a tokio runtime.
pub fn spawn_webhook_worker(engine: Weak<EngineHandle>) {
    tokio::spawn(async move {
        let client = build_http_client();
        let mut known_gen = match engine.upgrade() {
            Some(e) => e.event_commit_gen(),
            None => return, // engine already gone
        };
        loop {
            let Some(handle) = engine.upgrade() else {
                return; // AppState/EngineHandle dropped — nothing left to serve
            };
            known_gen = handle.wait_event_commit(known_gen, FALLBACK_INTERVAL).await;

            let webhooks = match handle.list_webhooks().await {
                Ok(w) => w,
                Err(_) => continue, // engine unavailable this tick — retry next
            };
            if webhooks.is_empty() {
                // No consumer offset touched — see the doc comment above.
                continue;
            }

            let Ok(xid) = handle.begin(None).await else {
                continue;
            };
            let poll_result = handle
                .poll_events(xid, CONSUMER_NAME.to_string(), POLL_LIMIT)
                .await;
            let _ = handle.commit(xid).await;
            let events = match poll_result {
                Ok(events) => events,
                Err(e) => {
                    tracing::warn!(error = %e, "webhook worker: poll_events failed this tick");
                    continue;
                }
            };
            if events.is_empty() {
                continue;
            }
            metrics::counter!("unidb_webhook_poll_cycles_total").increment(1);

            let max_seq = events.iter().map(|e| e.seq).max().unwrap_or(0);

            let mut delivery_tasks = Vec::new();
            for event in &events {
                for def in webhooks.iter().filter(|d| webhook_matches(d, event)) {
                    let body = crate::server::event_format::format_event(event, "native");
                    let signature = resolve_secret(&handle, def)
                        .await
                        .and_then(|secret| sign(&secret, body.as_bytes()));
                    let client = client.clone();
                    let def = def.clone();
                    delivery_tasks.push(tokio::spawn(async move {
                        deliver_with_retry(&client, &def, body, signature).await;
                    }));
                }
            }
            metrics::counter!("unidb_webhook_events_delivered_total")
                .increment(delivery_tasks.len() as u64);
            // Every delivery in this tick runs concurrently (see the module
            // doc) — this just waits for the slowest one (a dead endpoint's
            // exhausted retries) before acking, so the durable offset never
            // advances past a delivery still in-flight. It does NOT
            // serialize deliveries against each other; a `JoinError` (task
            // panic) is impossible here since `deliver_with_retry` itself
            // never panics.
            for task in delivery_tasks {
                let _ = task.await;
            }

            // Advance the durable offset past the whole batch regardless of
            // individual delivery outcomes (isolated failures above) — a
            // dead endpoint must never wedge the stream (backlog spec's
            // explicit AC). At-least-once: see the module doc's crash note.
            let Ok(ack_xid) = handle.begin(None).await else {
                continue;
            };
            let ack_result = handle
                .ack_events(ack_xid, CONSUMER_NAME.to_string(), max_seq)
                .await;
            let _ = handle.commit(ack_xid).await;
            if let Err(e) = ack_result {
                tracing::warn!(error = %e, "webhook worker: ack_events failed this tick");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn make_webhook(id: &str, table_pattern: &str, events: &[WebhookEvent]) -> WebhookDef {
        WebhookDef {
            id: id.to_string(),
            target_url: "http://127.0.0.1:0/hook".to_string(),
            table_pattern: table_pattern.to_string(),
            events: events.iter().copied().collect::<BTreeSet<_>>(),
            signing_secret: None,
            enabled: true,
            headers: Default::default(),
        }
    }

    fn make_event(table: &str, op: &str) -> Event {
        Event {
            seq: 1,
            xid: 1,
            table_name: table.to_string(),
            op: op.to_string(),
            payload: serde_json::json!({}),
            before: None,
            after: Some(serde_json::json!({"id": 1})),
            ts_ms: 0,
        }
    }

    #[test]
    fn matches_exact_table_and_scoped_event() {
        let def = make_webhook("w1", "orders", &[WebhookEvent::Insert]);
        assert!(webhook_matches(&def, &make_event("orders", "insert")));
        assert!(!webhook_matches(&def, &make_event("orders", "update")));
        assert!(!webhook_matches(&def, &make_event("other", "insert")));
    }

    #[test]
    fn wildcard_table_pattern_matches_every_table() {
        let def = make_webhook("w1", "*", &[WebhookEvent::Delete]);
        assert!(webhook_matches(&def, &make_event("orders", "delete")));
        assert!(webhook_matches(&def, &make_event("users", "delete")));
        assert!(!webhook_matches(&def, &make_event("orders", "insert")));
    }

    #[test]
    fn disabled_webhook_never_matches() {
        let mut def = make_webhook("w1", "*", &[WebhookEvent::Insert]);
        def.enabled = false;
        assert!(!webhook_matches(&def, &make_event("orders", "insert")));
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_test_vector_1() {
        // RFC 4231 §4.2, Test Case 1: key = 0x0b * 20, data = "Hi There".
        let key = [0x0bu8; 20];
        let digest = hmac_sha256(&key, b"Hi There");
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff"
        );
    }

    #[test]
    fn signature_is_deterministic_and_verifiable() {
        let body = b"{\"table\":\"orders\"}";
        let sig1 = sign("supersecret", body).unwrap();
        let sig2 = sign("supersecret", body).unwrap();
        assert_eq!(sig1, sig2);
        assert!(sig1.starts_with("sha256="));
        // A different secret must produce a different signature.
        let sig3 = sign("othersecret", body).unwrap();
        assert_ne!(sig1, sig3);
    }
}
