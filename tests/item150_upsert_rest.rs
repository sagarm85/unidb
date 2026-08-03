#![cfg(feature = "server")]
//! Item 150 — `POST /rest/v1/<table>` upsert wiring: `on_conflict=<col>` +
//! `Prefer: resolution=merge-duplicates|ignore-duplicates`. Removes item
//! 139's documented exclusion (`docs/backlog/139_rest_count_prefer.md`'s
//! note that PostgREST upsert needed `ON CONFLICT`, which the engine didn't
//! have yet). Follows `tests/item139_rest_count_prefer.rs`'s harness
//! pattern closely (same `TestServer`/bootstrap-superuser/`rest_*` helper
//! shapes).

#[path = "server_common/mod.rs"]
mod server_common;

use reqwest::header::HeaderMap;
use serde_json::{json, Value};
use server_common::{token_for, valid_token, TestServer};

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

struct RestResponse {
    status: u16,
    #[allow(dead_code)]
    headers: HeaderMap,
    body: String,
}

impl RestResponse {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("expected JSON body, got {:?}: {e}", self.body))
    }
}

/// `POST /rest/v1/<path>` where `path` may include a query string (e.g.
/// `"items?on_conflict=id"`) — mirrors `item139`'s `rest_patch`/`rest_get`
/// shape (which both take a full `path`), extended here to `POST` since
/// upsert needs the query string too.
async fn rest_post_q(
    server: &TestServer,
    token: &str,
    path: &str,
    prefer: Option<&str>,
    body: Value,
) -> RestResponse {
    let client = reqwest::Client::new();
    let mut req = client
        .post(server.url(&format!("/rest/v1/{path}")))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body);
    if let Some(p) = prefer {
        req = req.header("Prefer", p);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let text = resp.text().await.unwrap();
    RestResponse {
        status,
        headers,
        body: text,
    }
}

async fn rest_get(server: &TestServer, token: &str, path: &str) -> RestResponse {
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url(&format!("/rest/v1/{path}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let text = resp.text().await.unwrap();
    RestResponse {
        status,
        headers,
        body: text,
    }
}

fn rows(body: &Value) -> &Vec<Value> {
    body["rows"].as_array().unwrap()
}

/// Positional row value lookup by column name (the wire shape is
/// `{"columns":[...],"rows":[[...]]}` — positional arrays, not keyed
/// objects — mirrors `item139`'s `name_idx` pattern).
fn field<'a>(body: &'a Value, row_idx: usize, col_name: &str) -> &'a Value {
    let idx = body["columns"]
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c == col_name)
        .unwrap_or_else(|| panic!("column {col_name} not in {:?}", body["columns"]));
    &rows(body)[row_idx][idx]
}

/// `items(id INT PRIMARY KEY, name TEXT, hits INT)`, empty, owned by a fresh
/// superuser `root` — mirrors `item139`'s `setup_items_table` (bootstrap
/// pattern), with a PK so `on_conflict=id` has a real target.
async fn setup_items_table(server: &TestServer) -> String {
    let admin = valid_token();
    assert_eq!(
        sql(server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    let root = token_for("root");
    assert_eq!(
        sql(
            server,
            &root,
            "CREATE TABLE items (id INT PRIMARY KEY, name TEXT, hits INT)"
        )
        .await
        .0,
        200
    );
    root
}

// ── merge-duplicates ─────────────────────────────────────────────────────────

#[tokio::test]
async fn merge_duplicates_updates_existing_row_and_inserts_new() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;

    let r1 = rest_post_q(
        &server,
        &root,
        "items?on_conflict=id",
        Some("resolution=merge-duplicates"),
        json!({"id": 1, "name": "first", "hits": 10}),
    )
    .await;
    assert_eq!(r1.status, 200, "body: {}", r1.body);
    assert_eq!(r1.json()["type"], "inserted");

    // Conflicting row: merges (updates) instead of erroring.
    let r2 = rest_post_q(
        &server,
        &root,
        "items?on_conflict=id",
        Some("resolution=merge-duplicates"),
        json!({"id": 1, "name": "merged", "hits": 99}),
    )
    .await;
    assert_eq!(r2.status, 200, "body: {}", r2.body);

    let check = rest_get(&server, &root, "items?id=eq.1").await;
    let check_body = check.json();
    assert_eq!(field(&check_body, 0, "name"), "merged");
    assert_eq!(field(&check_body, 0, "hits"), 99);

    // A non-conflicting row in the same mode still just inserts.
    let r3 = rest_post_q(
        &server,
        &root,
        "items?on_conflict=id",
        Some("resolution=merge-duplicates"),
        json!({"id": 2, "name": "second", "hits": 1}),
    )
    .await;
    assert_eq!(r3.status, 200);
    let check_all = rest_get(&server, &root, "items?order=id.asc").await;
    assert_eq!(rows(&check_all.json()).len(), 2);
}

#[tokio::test]
async fn merge_duplicates_composes_with_return_representation() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    rest_post_q(
        &server,
        &root,
        "items?on_conflict=id",
        Some("resolution=merge-duplicates"),
        json!({"id": 1, "name": "first", "hits": 10}),
    )
    .await;

    let r = rest_post_q(
        &server,
        &root,
        "items?on_conflict=id",
        Some("resolution=merge-duplicates, return=representation"),
        json!({"id": 1, "name": "merged-repr", "hits": 42}),
    )
    .await;
    assert_eq!(r.status, 200, "body: {}", r.body);
    let body = r.json();
    assert_eq!(body["type"], "rows");
    assert_eq!(rows(&body).len(), 1);
    assert_eq!(field(&body, 0, "name"), "merged-repr");
}

#[tokio::test]
async fn merge_duplicates_without_on_conflict_param_is_a_client_error() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    let r = rest_post_q(
        &server,
        &root,
        "items", // no on_conflict= query param
        Some("resolution=merge-duplicates"),
        json!({"id": 1, "name": "x", "hits": 1}),
    )
    .await;
    assert_eq!(r.status, 400, "body: {}", r.body);
}

// ── ignore-duplicates ────────────────────────────────────────────────────────

#[tokio::test]
async fn ignore_duplicates_skips_existing_row_inserts_new() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    rest_post_q(
        &server,
        &root,
        "items?on_conflict=id",
        Some("resolution=merge-duplicates"),
        json!({"id": 1, "name": "original", "hits": 10}),
    )
    .await;

    let r = rest_post_q(
        &server,
        &root,
        "items?on_conflict=id",
        Some("resolution=ignore-duplicates"),
        json!({"id": 1, "name": "should_be_ignored", "hits": 999}),
    )
    .await;
    assert_eq!(r.status, 200, "body: {}", r.body);
    assert_eq!(r.json()["count"], 0);

    let check = rest_get(&server, &root, "items?id=eq.1").await;
    let check_body = check.json();
    assert_eq!(field(&check_body, 0, "name"), "original");

    let r2 = rest_post_q(
        &server,
        &root,
        "items?on_conflict=id",
        Some("resolution=ignore-duplicates"),
        json!({"id": 2, "name": "fresh", "hits": 1}),
    )
    .await;
    assert_eq!(r2.status, 200);
    assert_eq!(r2.json()["count"], 1);
}

#[tokio::test]
async fn ignore_duplicates_without_target_absorbs_any_unique_violation() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    assert_eq!(
        sql(&server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    let root = token_for("root");
    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE TABLE people (id INT PRIMARY KEY, email TEXT UNIQUE)"
        )
        .await
        .0,
        200
    );
    rest_post_q(
        &server,
        &root,
        "people",
        None,
        json!({"id": 1, "email": "a@x.com"}),
    )
    .await;

    // No on_conflict= param — DO NOTHING absorbs a conflict on ANY unique
    // column (here, `email`, not the omitted target).
    let r = rest_post_q(
        &server,
        &root,
        "people",
        Some("resolution=ignore-duplicates"),
        json!({"id": 2, "email": "a@x.com"}),
    )
    .await;
    assert_eq!(r.status, 200, "body: {}", r.body);
    assert_eq!(r.json()["count"], 0);
}

// ── no-Prefer regression: byte-identical pre-150 behavior ──────────────────

#[tokio::test]
async fn no_prefer_header_keeps_pre_150_conflict_behavior_even_with_on_conflict_param() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server).await;
    rest_post_q(
        &server,
        &root,
        "items",
        None,
        json!({"id": 1, "name": "original", "hits": 10}),
    )
    .await;

    // `on_conflict=id` present, but NO `Prefer: resolution=...` — must be
    // the plain pre-150 conflict error, not silently upserted.
    let r = rest_post_q(
        &server,
        &root,
        "items?on_conflict=id",
        None,
        json!({"id": 1, "name": "dup", "hits": 1}),
    )
    .await;
    assert_eq!(r.status, 409, "body: {}", r.body);

    let check = rest_get(&server, &root, "items?id=eq.1").await;
    let check_body = check.json();
    assert_eq!(
        field(&check_body, 0, "name"),
        "original",
        "the rejected conflicting write must not have applied"
    );
}

// ── RLS parity ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn merge_duplicates_respects_update_rls_policy() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    assert_eq!(
        sql(&server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    let root = token_for("root");
    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE TABLE docs (id INT PRIMARY KEY, owner TEXT, val INT)"
        )
        .await
        .0,
        200
    );
    assert_eq!(sql(&server, &root, "CREATE USER alice").await.0, 200);
    assert_eq!(sql(&server, &root, "CREATE USER bob").await.0, 200);
    assert_eq!(
        sql(
            &server,
            &root,
            "GRANT SELECT, INSERT, UPDATE ON docs TO alice"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "GRANT SELECT, INSERT, UPDATE ON docs TO bob"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE POLICY ins_own ON docs FOR INSERT USING (owner = current_user)"
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE POLICY upd_own ON docs FOR UPDATE USING (owner = current_user)"
        )
        .await
        .0,
        200
    );

    let bob = token_for("bob");
    assert_eq!(
        sql(&server, &bob, "INSERT INTO docs VALUES (1, 'bob', 10)")
            .await
            .0,
        200
    );

    // alice tries to upsert-merge into bob's row (same PK) — the update
    // arm's USING (owner = current_user) does not match bob's row, so this
    // must be rejected, not silently merged (same fail-closed contract the
    // engine-level `rls_update_arm_using_mismatch_is_an_error_not_a_silent_skip`
    // test proves directly against the engine).
    let alice = token_for("alice");
    let r = rest_post_q(
        &server,
        &alice,
        "docs?on_conflict=id",
        Some("resolution=merge-duplicates"),
        json!({"id": 1, "owner": "alice", "val": 5}),
    )
    .await;
    assert!(
        r.status == 400 || r.status == 403,
        "expected a client error (RLS USING mismatch), got {} body: {}",
        r.status,
        r.body
    );

    // bob's row is untouched.
    let check = rest_get(&server, &bob, "docs?id=eq.1").await;
    let check_body = check.json();
    assert_eq!(field(&check_body, 0, "val"), 10);
}
