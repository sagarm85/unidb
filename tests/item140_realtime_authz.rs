#![cfg(feature = "server")]
//! Item 140: Realtime channel authorization — the item-132 follow-up that
//! adds an RLS-style, role-based, topic-glob channel-policy gate ahead of
//! all four realtime routes (`broadcast/publish`, `broadcast/subscribe`,
//! `presence/subscribe`, `presence/track`), plus the superuser-only
//! `/realtime/policies` admin surface.
//!
//! Follows the `tests/item132_realtime.rs` harness pattern (`TestServer`/
//! `token_for`/`token_with_claims` from `tests/server_common`).
//!
//! **`UNIDB_REALTIME_REQUIRE_AUTHZ` is read per-request (control-plane, not
//! cached at server startup)** — it is a real process-global env var, so the
//! one test here that toggles it (`require_authz_on_denies_unmatched_topic`)
//! holds `ENV_GUARD` for its **entire** async body (not just the
//! set/unset calls) to serialize against every other test in this binary
//! that spawns a server and expects the default (off) posture. Every other
//! test in this file also takes the guard, purely so a future test added
//! here without reading this comment can't reintroduce the race.

#[path = "server_common/mod.rs"]
mod server_common;

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{json, Value};
use server_common::{token_for, token_with_claims, valid_token, TestServer};
use tokio::sync::Mutex;

/// Serializes every test in this binary against
/// `UNIDB_REALTIME_REQUIRE_AUTHZ` mutation — see the module doc. An
/// async-aware `tokio::sync::Mutex` (not `std::sync::Mutex`), since every
/// test holds this guard across `.await` points for its entire body.
static ENV_GUARD: Mutex<()> = Mutex::const_new(());

async fn sql(server: &TestServer, token: &str, sql: &str) -> (u16, Value) {
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "sql": sql }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn put_policy(
    server: &TestServer,
    token: &str,
    topic_pattern: &str,
    operation: &str,
    roles: &[&str],
) -> u16 {
    reqwest::Client::new()
        .put(server.url("/realtime/policies"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({
            "topic_pattern": topic_pattern,
            "operation": operation,
            "roles": roles,
        }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn delete_policy(
    server: &TestServer,
    token: &str,
    topic_pattern: &str,
    operation: &str,
) -> u16 {
    reqwest::Client::new()
        .delete(server.url("/realtime/policies"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "topic_pattern": topic_pattern, "operation": operation }))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn get_policies(server: &TestServer, token: &str) -> (u16, Value) {
    let resp = reqwest::Client::new()
        .get(server.url("/realtime/policies"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn subscribe_broadcast(server: &TestServer, token: &str, topic: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(server.url(&format!("/realtime/broadcast/subscribe?topic={topic}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
}

async fn subscribe_presence(server: &TestServer, token: &str, topic: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(server.url(&format!("/realtime/presence/subscribe?topic={topic}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
}

async fn publish(server: &TestServer, token: &str, topic: &str) -> u16 {
    reqwest::Client::new()
        .post(server.url("/realtime/broadcast/publish"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({"topic": topic, "event": "msg", "payload": {}}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn track(server: &TestServer, token: &str, topic: &str) -> u16 {
    reqwest::Client::new()
        .post(server.url("/realtime/presence/track"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({"topic": topic, "key": "k", "state": {}}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// Bootstraps a `root` superuser, `member` role, `alice` (a member) and
/// `bob` (an authenticated non-member) — the standard cast for the
/// role-based policy tests below.
async fn setup_root_member_and_bob(server: &TestServer) -> String {
    let bootstrap = valid_token();
    assert_eq!(
        sql(server, &bootstrap, "CREATE USER root SUPERUSER")
            .await
            .0,
        200
    );
    let root = token_for("root");
    assert_eq!(sql(server, &root, "CREATE ROLE member").await.0, 200);
    assert_eq!(sql(server, &root, "CREATE USER alice").await.0, 200);
    assert_eq!(sql(server, &root, "GRANT member TO alice").await.0, 200);
    assert_eq!(sql(server, &root, "CREATE USER bob").await.0, 200);
    root
}

// ── (a) default posture: REQUIRE_AUTHZ off + no policy = item-132 behavior ─

#[tokio::test]
async fn require_authz_off_no_policy_behaves_like_item132() {
    let _guard = ENV_GUARD.lock().await;
    let server = TestServer::spawn().await;
    let token = valid_token();

    let resp = subscribe_broadcast(&server, &token, "unrestricted-topic").await;
    assert_eq!(resp.status(), 200);
    drop(resp);

    let resp = subscribe_presence(&server, &token, "unrestricted-topic").await;
    assert_eq!(resp.status(), 200);
    drop(resp);

    assert_eq!(publish(&server, &token, "unrestricted-topic").await, 200);
    assert_eq!(track(&server, &token, "unrestricted-topic").await, 204);
}

// ── (b) a matching policy restricts subscribe to the granted role ──────────

#[tokio::test]
async fn policy_restricts_subscribe_member_allowed_nonmember_403() {
    let _guard = ENV_GUARD.lock().await;
    let server = TestServer::spawn().await;
    let root = setup_root_member_and_bob(&server).await;

    assert_eq!(
        put_policy(&server, &root, "room:*", "subscribe", &["member"]).await,
        204
    );

    let alice = token_for("alice");
    let bob = token_for("bob");

    let alice_resp = subscribe_broadcast(&server, &alice, "room:1").await;
    assert_eq!(
        alice_resp.status(),
        200,
        "a member must be allowed to subscribe"
    );
    drop(alice_resp);

    let bob_resp = subscribe_broadcast(&server, &bob, "room:1").await;
    assert_eq!(
        bob_resp.status(),
        403,
        "a non-member must be denied with 403"
    );
}

// ── (c) service_role bypasses a restrictive policy, audited ────────────────

#[tokio::test]
async fn service_role_bypasses_restricted_topic_and_is_audited() {
    let _guard = ENV_GUARD.lock().await;
    let server = TestServer::spawn().await;
    let root = setup_root_member_and_bob(&server).await;
    assert_eq!(
        put_policy(&server, &root, "room:*", "subscribe", &["member"]).await,
        204
    );

    let service_role_token = token_with_claims(None, &[("role", json!("service_role"))]);
    let resp = subscribe_broadcast(&server, &service_role_token, "room:99").await;
    assert_eq!(
        resp.status(),
        200,
        "service_role must bypass the channel policy entirely"
    );
    drop(resp);

    let audit = std::fs::read_to_string(server.data_dir().join("audit.log")).unwrap();
    assert!(
        audit
            .lines()
            .any(|l| l.contains("\"action\":\"service_role_rls_bypass\"")
                && l.contains("\"object\":\"realtime/broadcast/subscribe\"")),
        "service_role realtime channel bypass must be audited; audit.log:\n{audit}"
    );
}

// ── (d) REQUIRE_AUTHZ on + no matching policy => 403 (fail-closed) ─────────

#[tokio::test]
async fn require_authz_on_denies_unmatched_topic() {
    let _guard = ENV_GUARD.lock().await;
    std::env::set_var("UNIDB_REALTIME_REQUIRE_AUTHZ", "1");

    let server = TestServer::spawn().await;
    // A registered, *non*-superuser caller: in open/bootstrap mode (no users
    // registered at all) every caller — including `valid_token()`'s bare
    // `sub` — is treated as an implicit superuser and would bypass the gate
    // outright, masking the enforce-mode behavior this test targets.
    let bootstrap = valid_token();
    assert_eq!(
        sql(&server, &bootstrap, "CREATE USER root SUPERUSER")
            .await
            .0,
        200
    );
    let root = token_for("root");
    assert_eq!(sql(&server, &root, "CREATE USER plain").await.0, 200);
    let plain = token_for("plain");

    let resp = subscribe_broadcast(&server, &plain, "no-policy-topic").await;
    let status = resp.status();
    drop(resp);

    std::env::remove_var("UNIDB_REALTIME_REQUIRE_AUTHZ");

    assert_eq!(
        status, 403,
        "enforce mode must deny a topic with no matching policy"
    );
}

// ── (e) most-specific pattern wins ──────────────────────────────────────────

#[tokio::test]
async fn most_specific_pattern_wins_over_glob() {
    let _guard = ENV_GUARD.lock().await;
    let server = TestServer::spawn().await;
    let root = setup_root_member_and_bob(&server).await;
    assert_eq!(sql(&server, &root, "CREATE ROLE vip").await.0, 200);
    assert_eq!(sql(&server, &root, "CREATE USER carol").await.0, 200);
    assert_eq!(sql(&server, &root, "GRANT vip TO carol").await.0, 200);

    assert_eq!(
        put_policy(&server, &root, "room:*", "subscribe", &["member"]).await,
        204
    );
    assert_eq!(
        put_policy(&server, &root, "room:42", "subscribe", &["vip"]).await,
        204
    );

    let alice = token_for("alice"); // member
    let carol = token_for("carol"); // vip

    // The exact "room:42" policy overrides "room:*" for that one topic: a
    // plain member (allowed under the glob) is now denied there.
    let alice_42 = subscribe_broadcast(&server, &alice, "room:42").await;
    assert_eq!(
        alice_42.status(),
        403,
        "exact-match policy must override the glob for room:42"
    );
    drop(alice_42);

    let carol_42 = subscribe_broadcast(&server, &carol, "room:42").await;
    assert_eq!(carol_42.status(), 200, "vip must be allowed on room:42");
    drop(carol_42);

    // A different topic under the glob still goes through the less-specific
    // "room:*" policy, so a plain member is still allowed there.
    let alice_7 = subscribe_broadcast(&server, &alice, "room:7").await;
    assert_eq!(
        alice_7.status(),
        200,
        "member must still be allowed on a topic only the glob covers"
    );
}

// ── (f) operation scoping: a publish-only policy doesn't gate subscribe ────

#[tokio::test]
async fn operation_scoping_publish_policy_does_not_gate_subscribe() {
    let _guard = ENV_GUARD.lock().await;
    let server = TestServer::spawn().await;
    let root = setup_root_member_and_bob(&server).await;
    assert_eq!(
        put_policy(&server, &root, "chan1", "publish", &["member"]).await,
        204
    );

    let alice = token_for("alice");
    let bob = token_for("bob");

    assert_eq!(
        publish(&server, &alice, "chan1").await,
        200,
        "member must be allowed to publish"
    );
    assert_eq!(
        publish(&server, &bob, "chan1").await,
        403,
        "non-member must be denied publish"
    );

    // No `subscribe` policy matches "chan1" — default-off posture still
    // allows any authenticated principal to subscribe.
    let bob_sub = subscribe_broadcast(&server, &bob, "chan1").await;
    assert_eq!(
        bob_sub.status(),
        200,
        "a publish-scoped policy must not gate subscribe on the same topic"
    );
}

// ── (g) presence op covers both presence/subscribe and presence/track ──────

#[tokio::test]
async fn presence_op_covers_subscribe_and_track() {
    let _guard = ENV_GUARD.lock().await;
    let server = TestServer::spawn().await;
    let root = setup_root_member_and_bob(&server).await;
    assert_eq!(
        put_policy(&server, &root, "p:*", "presence", &["member"]).await,
        204
    );

    let alice = token_for("alice");
    let bob = token_for("bob");

    let alice_resp = subscribe_presence(&server, &alice, "p:1").await;
    assert_eq!(alice_resp.status(), 200);
    drop(alice_resp);

    let bob_resp = subscribe_presence(&server, &bob, "p:1").await;
    assert_eq!(bob_resp.status(), 403);

    assert_eq!(track(&server, &alice, "p:1").await, 204);
    assert_eq!(track(&server, &bob, "p:1").await, 403);
}

// ── (h) management routes are superuser-only ────────────────────────────────

#[tokio::test]
async fn management_routes_are_superuser_only() {
    let _guard = ENV_GUARD.lock().await;
    let server = TestServer::spawn().await;
    let bootstrap = valid_token();
    assert_eq!(
        sql(&server, &bootstrap, "CREATE USER root SUPERUSER")
            .await
            .0,
        200
    );
    let root = token_for("root");
    assert_eq!(sql(&server, &root, "CREATE USER plain").await.0, 200);
    let plain = token_for("plain");

    // Non-superuser -> 403 on all three admin routes.
    assert_eq!(
        put_policy(&server, &plain, "t", "subscribe", &["member"]).await,
        403
    );
    assert_eq!(delete_policy(&server, &plain, "t", "subscribe").await, 403);
    let (status, _) = get_policies(&server, &plain).await;
    assert_eq!(status, 403);

    // Superuser -> succeeds end to end: put, list, delete, list again.
    assert_eq!(
        put_policy(&server, &root, "t", "subscribe", &["member"]).await,
        204
    );
    let (status, body) = get_policies(&server, &root).await;
    assert_eq!(status, 200);
    let arr = body.as_array().unwrap();
    assert!(
        arr.iter()
            .any(|p| p["topic_pattern"] == "t" && p["operation"] == "subscribe"),
        "expected the upserted policy to be listed: {body}"
    );

    assert_eq!(delete_policy(&server, &root, "t", "subscribe").await, 204);
    let (status, body) = get_policies(&server, &root).await;
    assert_eq!(status, 200);
    assert!(
        body.as_array().unwrap().is_empty(),
        "policy must be gone after delete: {body}"
    );

    // Deleting an already-absent policy is idempotent, not an error.
    assert_eq!(delete_policy(&server, &root, "t", "subscribe").await, 204);
}

// ── (i) invalid operation string is a clean 400, not a panic ───────────────

#[tokio::test]
async fn invalid_operation_string_is_bad_request() {
    let _guard = ENV_GUARD.lock().await;
    let server = TestServer::spawn().await;
    let bootstrap = valid_token();
    assert_eq!(
        sql(&server, &bootstrap, "CREATE USER root SUPERUSER")
            .await
            .0,
        200
    );
    let root = token_for("root");
    assert_eq!(
        put_policy(&server, &root, "t", "not-a-real-op", &["member"]).await,
        400
    );
}

// Keep the collector import alive without pulling in a full SSE frame parser
// here — the tests above only need HTTP status codes, but confirming a 200
// response is actually a live SSE stream (not an empty body masquerading as
// success) is worth one lightweight smoke check.
#[tokio::test]
async fn allowed_subscribe_actually_streams_frames() {
    let _guard = ENV_GUARD.lock().await;
    let server = TestServer::spawn().await;
    let root = setup_root_member_and_bob(&server).await;
    assert_eq!(
        put_policy(&server, &root, "room:*", "subscribe", &["member"]).await,
        204
    );
    let alice = token_for("alice");
    let resp = subscribe_broadcast(&server, &alice, "room:5").await;
    assert_eq!(resp.status(), 200);

    assert_eq!(publish(&server, &root, "room:5").await, 200);

    let mut stream = resp.bytes_stream();
    let got = tokio::time::timeout(Duration::from_secs(3), stream.next()).await;
    assert!(
        matches!(got, Ok(Some(Ok(_)))),
        "an allowed subscriber must actually receive stream bytes"
    );
}
