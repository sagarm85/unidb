#![cfg(feature = "server")]
//! Item 141 — Database webhooks (outbound HTTP on row change).
//!
//! Follows the `tests/item128_oauth.rs`/`tests/item140_realtime_authz.rs`
//! harness pattern: a real `unidb-server` (`TestServer`) plus a tiny local
//! axum "mock receiver" standing in for the operator's HTTP endpoint — no
//! real network, both sides bind to `127.0.0.1:0` (ephemeral loopback ports
//! only).
//!
//! Test matrix (mirrors the backlog doc's acceptance list):
//!   (a) insert_delivers_matching_event_with_valid_signature
//!   (b) update_and_delete_deliver_correct_before_after_images
//!   (c) insert_scoped_webhook_ignores_updates
//!   (d) wildcard_table_pattern_matches_every_table
//!   (e) failing_endpoint_is_retried_then_skipped_without_blocking_healthy_webhook
//!   (f) secrets_never_appear_in_get_webhooks
//!   (g) admin_routes_are_superuser_only
//!   (h) disabled_webhook_receives_nothing + delete_removes_the_registration

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    body::Bytes, extract::State as AxumState, http::HeaderMap, response::IntoResponse,
    routing::post, Router,
};
use serde_json::{json, Value};

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::{token_for, valid_token, TestServer};

// ── a tiny local mock webhook receiver (no real network) ───────────────────

#[derive(Clone, Debug)]
struct Received {
    body: Value,
    signature: Option<String>,
}

#[derive(Clone, Default)]
struct ReceiverState {
    inner: Arc<Mutex<Vec<Received>>>,
}

async fn receive_hook(
    AxumState(state): AxumState<ReceiverState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let signature = headers
        .get("x-unidb-signature")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state
        .inner
        .lock()
        .unwrap()
        .push(Received { body, signature });
    axum::http::StatusCode::OK
}

struct MockReceiver {
    addr: std::net::SocketAddr,
    state: ReceiverState,
    _task: tokio::task::JoinHandle<()>,
}

impl MockReceiver {
    async fn spawn() -> Self {
        let state = ReceiverState::default();
        let router = Router::new()
            .route("/hook", post(receive_hook))
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

    fn hook_url(&self) -> String {
        format!("http://{}/hook", self.addr)
    }

    fn received(&self) -> Vec<Received> {
        self.state.inner.lock().unwrap().clone()
    }
}

/// A target URL guaranteed to refuse connections immediately: bind an
/// ephemeral loopback port, then drop the listener before returning — the
/// port is free again, so a subsequent connect fails fast with "connection
/// refused" instead of timing out. Used to drive the failing-endpoint test
/// deterministically and quickly (no real dead host, no long OS-level
/// connect timeout).
async fn dead_target_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/nope")
}

/// Poll `pred` every 20ms until it's true or `timeout` elapses. Returns
/// whether `pred` became true in time.
async fn wait_for(mut pred: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ── shared HTTP helpers ─────────────────────────────────────────────────────

fn client() -> reqwest::Client {
    reqwest::Client::new()
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

async fn enable_events(server: &TestServer, token: &str, table: &str) {
    let resp = client()
        .post(server.url(&format!("/tables/{table}/events")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

/// Bootstraps a `root` superuser — the standard cast for the admin-route
/// tests below (mirrors `item140_realtime_authz.rs::setup_root_member_and_bob`).
async fn setup_root(server: &TestServer) -> String {
    let bootstrap = valid_token();
    assert_eq!(
        sql(server, &bootstrap, "CREATE USER root SUPERUSER")
            .await
            .0,
        200
    );
    token_for("root")
}

async fn register_webhook(server: &TestServer, token: &str, body: Value) -> u16 {
    client()
        .post(server.url("/webhooks"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn list_webhooks(server: &TestServer, token: &str) -> (u16, Value) {
    let resp = client()
        .get(server.url("/webhooks"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn delete_webhook(server: &TestServer, token: &str, id: &str) -> u16 {
    client()
        .delete(server.url(&format!("/webhooks/{id}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn metrics_body(server: &TestServer) -> String {
    client()
        .get(server.url("/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

// ── (a) insert delivers the matching event with a valid signature ──────────

#[tokio::test]
async fn insert_delivers_matching_event_with_valid_signature() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;
    let receiver = MockReceiver::spawn().await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE orders (id INT, name TEXT)")
            .await
            .0,
        200
    );
    enable_events(&server, &root, "orders").await;

    assert_eq!(
        register_webhook(
            &server,
            &root,
            json!({
                "id": "w1",
                "target_url": receiver.hook_url(),
                "table_pattern": "orders",
                "events": ["insert"],
                "signing_secret": "topsecret",
            }),
        )
        .await,
        204
    );

    assert_eq!(
        sql(
            &server,
            &root,
            "INSERT INTO orders (id, name) VALUES (1, 'widget')"
        )
        .await
        .0,
        200
    );

    let got = wait_for(|| !receiver.received().is_empty(), Duration::from_secs(5)).await;
    assert!(got, "webhook receiver never got a delivery");

    let received = receiver.received();
    assert_eq!(
        received.len(),
        1,
        "expected exactly one delivery: {received:?}"
    );
    let delivery = &received[0];
    assert_eq!(delivery.body["table_name"], "orders");
    assert_eq!(delivery.body["op"], "insert");
    assert!(delivery.body["before"].is_null());
    assert_eq!(delivery.body["after"]["name"], "widget");

    // The wire-format shape of the signature (`sha256=<64 hex chars>`) is
    // checked here; the next test (`signature_is_independently_verifiable_
    // against_the_raw_wire_body`) recomputes the HMAC over the exact raw
    // bytes received and asserts byte-for-byte equality.
    let sig = delivery
        .signature
        .as_ref()
        .expect("delivery must carry X-Unidb-Signature");
    assert!(sig.starts_with("sha256="));
    assert_eq!(sig.len(), "sha256=".len() + 64);
}

// ── (a2) the signature is independently verifiable against the raw body ────

#[tokio::test]
async fn signature_is_independently_verifiable_against_the_raw_wire_body() {
    // Captures the *raw* bytes (not re-serialized JSON) so the HMAC can be
    // recomputed and compared byte-for-byte — proves the header really does
    // sign the exact body that was sent, not just "a signature shaped like
    // one is present" (the previous test's weaker check).
    type RawCapture = Vec<(Bytes, Option<String>)>;

    #[derive(Clone, Default)]
    struct RawState {
        inner: Arc<Mutex<RawCapture>>,
    }
    async fn capture_raw(
        AxumState(state): AxumState<RawState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        let sig = headers
            .get("x-unidb-signature")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        state.inner.lock().unwrap().push((body, sig));
        axum::http::StatusCode::OK
    }

    let state = RawState::default();
    let router = Router::new()
        .route("/hook", post(capture_raw))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _task = tokio::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service()).await;
    });

    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;
    assert_eq!(
        sql(&server, &root, "CREATE TABLE orders (id INT)").await.0,
        200
    );
    enable_events(&server, &root, "orders").await;
    assert_eq!(
        register_webhook(
            &server,
            &root,
            json!({
                "id": "w1",
                "target_url": format!("http://{addr}/hook"),
                "table_pattern": "orders",
                "events": ["insert"],
                "signing_secret": "my-signing-secret",
            }),
        )
        .await,
        204
    );
    assert_eq!(
        sql(&server, &root, "INSERT INTO orders (id) VALUES (1)")
            .await
            .0,
        200
    );

    let got = wait_for(
        || !state.inner.lock().unwrap().is_empty(),
        Duration::from_secs(5),
    )
    .await;
    assert!(got, "no delivery received");

    let (raw_body, signature) = state.inner.lock().unwrap()[0].clone();
    let expected = unidb::server::webhooks::sign("my-signing-secret", &raw_body);
    assert_eq!(
        signature, expected,
        "signature must match HMAC(secret, exact raw body)"
    );
}

// ── (b) update/delete deliver correct before/after images ──────────────────

#[tokio::test]
async fn update_and_delete_deliver_correct_before_after_images() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;
    let receiver = MockReceiver::spawn().await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE orders (id INT, name TEXT)")
            .await
            .0,
        200
    );
    enable_events(&server, &root, "orders").await;
    assert_eq!(
        register_webhook(
            &server,
            &root,
            json!({
                "id": "w1",
                "target_url": receiver.hook_url(),
                "table_pattern": "orders",
                "events": ["insert", "update", "delete"],
            }),
        )
        .await,
        204
    );

    assert_eq!(
        sql(
            &server,
            &root,
            "INSERT INTO orders (id, name) VALUES (1, 'a')"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(&server, &root, "UPDATE orders SET name = 'b' WHERE id = 1")
            .await
            .0,
        200
    );
    assert_eq!(
        sql(&server, &root, "DELETE FROM orders WHERE id = 1")
            .await
            .0,
        200
    );

    let got = wait_for(|| receiver.received().len() >= 3, Duration::from_secs(5)).await;
    assert!(got, "expected 3 deliveries, got: {:?}", receiver.received());

    let received = receiver.received();
    let insert = received.iter().find(|r| r.body["op"] == "insert").unwrap();
    assert!(insert.body["before"].is_null());
    assert_eq!(insert.body["after"]["name"], "a");

    let update = received.iter().find(|r| r.body["op"] == "update").unwrap();
    assert_eq!(update.body["before"]["name"], "a");
    assert_eq!(update.body["after"]["name"], "b");

    let delete = received.iter().find(|r| r.body["op"] == "delete").unwrap();
    assert_eq!(delete.body["before"]["name"], "b");
    assert!(delete.body["after"].is_null());
}

// ── (c) an [insert]-scoped webhook does not receive updates ────────────────

#[tokio::test]
async fn insert_scoped_webhook_ignores_updates() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;
    let receiver = MockReceiver::spawn().await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE orders (id INT, name TEXT)")
            .await
            .0,
        200
    );
    enable_events(&server, &root, "orders").await;
    assert_eq!(
        register_webhook(
            &server,
            &root,
            json!({
                "id": "w1",
                "target_url": receiver.hook_url(),
                "table_pattern": "orders",
                "events": ["insert"],
            }),
        )
        .await,
        204
    );

    assert_eq!(
        sql(
            &server,
            &root,
            "INSERT INTO orders (id, name) VALUES (1, 'a')"
        )
        .await
        .0,
        200
    );
    let got = wait_for(|| !receiver.received().is_empty(), Duration::from_secs(5)).await;
    assert!(got, "expected the insert to be delivered");
    assert_eq!(receiver.received().len(), 1);

    assert_eq!(
        sql(&server, &root, "UPDATE orders SET name = 'b' WHERE id = 1")
            .await
            .0,
        200
    );
    // Give the worker ample time to (incorrectly) deliver the update, then
    // assert it never showed up — a negative wait, not a race.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        receiver.received().len(),
        1,
        "an [insert]-scoped webhook must not receive an update event: {:?}",
        receiver.received()
    );
}

// ── (d) a `*` table_pattern matches every table ─────────────────────────────

#[tokio::test]
async fn wildcard_table_pattern_matches_every_table() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;
    let receiver = MockReceiver::spawn().await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE orders (id INT)").await.0,
        200
    );
    assert_eq!(
        sql(&server, &root, "CREATE TABLE users (id INT)").await.0,
        200
    );
    enable_events(&server, &root, "orders").await;
    enable_events(&server, &root, "users").await;

    assert_eq!(
        register_webhook(
            &server,
            &root,
            json!({
                "id": "w1",
                "target_url": receiver.hook_url(),
                "table_pattern": "*",
                "events": ["insert"],
            }),
        )
        .await,
        204
    );

    assert_eq!(
        sql(&server, &root, "INSERT INTO orders (id) VALUES (1)")
            .await
            .0,
        200
    );
    assert_eq!(
        sql(&server, &root, "INSERT INTO users (id) VALUES (1)")
            .await
            .0,
        200
    );

    let got = wait_for(|| receiver.received().len() >= 2, Duration::from_secs(5)).await;
    assert!(got, "got: {:?}", receiver.received());
    let tables: Vec<Value> = receiver
        .received()
        .into_iter()
        .map(|r| r.body["table_name"].clone())
        .collect();
    assert!(tables.contains(&json!("orders")));
    assert!(tables.contains(&json!("users")));
}

// ── (e) a failing endpoint is retried then skipped, without blocking a
//        second, healthy webhook ──────────────────────────────────────────

#[tokio::test]
async fn failing_endpoint_is_retried_then_skipped_without_blocking_healthy_webhook() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;
    let receiver = MockReceiver::spawn().await;
    let dead_url = dead_target_url().await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE orders (id INT)").await.0,
        200
    );
    enable_events(&server, &root, "orders").await;

    assert_eq!(
        register_webhook(
            &server,
            &root,
            json!({
                "id": "dead",
                "target_url": dead_url,
                "table_pattern": "orders",
                "events": ["insert"],
            }),
        )
        .await,
        204
    );
    assert_eq!(
        register_webhook(
            &server,
            &root,
            json!({
                "id": "healthy",
                "target_url": receiver.hook_url(),
                "table_pattern": "orders",
                "events": ["insert"],
            }),
        )
        .await,
        204
    );

    assert_eq!(
        sql(&server, &root, "INSERT INTO orders (id) VALUES (1)")
            .await
            .0,
        200
    );

    // The healthy webhook must not be blocked by the dead one — it should
    // receive its delivery quickly, well before the dead one exhausts its
    // bounded retries.
    let got = wait_for(|| !receiver.received().is_empty(), Duration::from_secs(3)).await;
    assert!(
        got,
        "the healthy webhook must receive its delivery without waiting on the dead one"
    );

    // The dead endpoint's retries eventually exhaust and increment the
    // failure metric — never propagates as a panic or an error anywhere
    // visible, only the counter. `wait_for`'s predicate is synchronous, so
    // poll `/metrics` directly in an async loop instead.
    let mut metric_seen = false;
    for _ in 0..100 {
        let body = metrics_body(&server).await;
        if body.contains("unidb_webhook_delivery_failures_total") {
            metric_seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        metric_seen,
        "expected unidb_webhook_delivery_failures_total to appear in /metrics after the dead \
         endpoint exhausts its retries"
    );

    // Exactly one delivery reached the healthy receiver (the dead webhook
    // never delivers anything there).
    assert_eq!(receiver.received().len(), 1);
}

// ── (f) secrets never appear in GET /webhooks ───────────────────────────────

#[tokio::test]
async fn secrets_never_appear_in_get_webhooks() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;

    assert_eq!(
        register_webhook(
            &server,
            &root,
            json!({
                "id": "w1",
                "target_url": "http://127.0.0.1:0/hook",
                "table_pattern": "orders",
                "events": ["insert"],
                "signing_secret": "extremely-secret-value",
            }),
        )
        .await,
        204
    );

    let (status, body) = list_webhooks(&server, &root).await;
    assert_eq!(status, 200);
    let text = body.to_string();
    assert!(
        !text.contains("extremely-secret-value"),
        "GET /webhooks must never leak the signing secret: {body}"
    );
    let arr = body.as_array().unwrap();
    let w1 = arr.iter().find(|w| w["id"] == "w1").unwrap();
    assert_eq!(w1["has_signing_secret"], true);
    assert!(w1.get("signing_secret").is_none());
}

// ── (g) admin routes are superuser-only ─────────────────────────────────────

#[tokio::test]
async fn admin_routes_are_superuser_only() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;
    assert_eq!(sql(&server, &root, "CREATE USER plain").await.0, 200);
    let plain = token_for("plain");

    let body = json!({
        "id": "w1",
        "target_url": "http://127.0.0.1:0/hook",
        "table_pattern": "orders",
        "events": ["insert"],
    });
    assert_eq!(register_webhook(&server, &plain, body.clone()).await, 403);
    let (status, _) = list_webhooks(&server, &plain).await;
    assert_eq!(status, 403);
    assert_eq!(delete_webhook(&server, &plain, "w1").await, 403);

    // Superuser succeeds end to end: create, list, delete, list again.
    assert_eq!(register_webhook(&server, &root, body).await, 204);
    let (status, listed) = list_webhooks(&server, &root).await;
    assert_eq!(status, 200);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    assert_eq!(delete_webhook(&server, &root, "w1").await, 204);
    let (status, listed) = list_webhooks(&server, &root).await;
    assert_eq!(status, 200);
    assert!(listed.as_array().unwrap().is_empty());

    // Deleting an already-absent webhook is idempotent, not an error.
    assert_eq!(delete_webhook(&server, &root, "w1").await, 204);
}

// ── (h) a disabled webhook receives nothing; delete stops delivery ─────────

#[tokio::test]
async fn disabled_webhook_receives_nothing() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;
    let receiver = MockReceiver::spawn().await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE orders (id INT)").await.0,
        200
    );
    enable_events(&server, &root, "orders").await;
    assert_eq!(
        register_webhook(
            &server,
            &root,
            json!({
                "id": "w1",
                "target_url": receiver.hook_url(),
                "table_pattern": "orders",
                "events": ["insert"],
                "enabled": false,
            }),
        )
        .await,
        204
    );

    assert_eq!(
        sql(&server, &root, "INSERT INTO orders (id) VALUES (1)")
            .await
            .0,
        200
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        receiver.received().is_empty(),
        "a disabled webhook must not receive deliveries: {:?}",
        receiver.received()
    );
}

#[tokio::test]
async fn delete_stops_further_delivery() {
    let server = TestServer::spawn().await;
    let root = setup_root(&server).await;
    let receiver = MockReceiver::spawn().await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE orders (id INT)").await.0,
        200
    );
    enable_events(&server, &root, "orders").await;
    assert_eq!(
        register_webhook(
            &server,
            &root,
            json!({
                "id": "w1",
                "target_url": receiver.hook_url(),
                "table_pattern": "orders",
                "events": ["insert"],
            }),
        )
        .await,
        204
    );

    assert_eq!(
        sql(&server, &root, "INSERT INTO orders (id) VALUES (1)")
            .await
            .0,
        200
    );
    let got = wait_for(|| !receiver.received().is_empty(), Duration::from_secs(5)).await;
    assert!(got);
    assert_eq!(receiver.received().len(), 1);

    assert_eq!(delete_webhook(&server, &root, "w1").await, 204);
    assert_eq!(
        sql(&server, &root, "INSERT INTO orders (id) VALUES (2)")
            .await
            .0,
        200
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        receiver.received().len(),
        1,
        "no further deliveries after the webhook is deleted: {:?}",
        receiver.received()
    );
}
