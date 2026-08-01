#![cfg(feature = "server")]
//! Item 139: `/rest/v1` exact count (`Prefer: count=exact` -> `Content-Range`)
//! and mutation response controls (`Prefer: return=representation|minimal`
//! on `POST`/`PATCH`/`DELETE`). Pure REST-layer response shaping — every
//! request still runs through the same enforced `run_stmt` path
//! (`src/server/rest_resource.rs`'s module doc), so RLS/grants apply to the
//! COUNT query exactly as they do to the main query.

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

/// One HTTP response captured in full: status, headers (for `Content-Range`/
/// `Preference-Applied` assertions), and the raw body text (`.text()`, not
/// `.json()` — a `204`/`201` `return=minimal` response has an empty body,
/// which `.json()` would fail to parse).
struct RestResponse {
    status: u16,
    headers: HeaderMap,
    body: String,
}

impl RestResponse {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("expected JSON body, got {:?}: {e}", self.body))
    }

    fn content_range(&self) -> Option<&str> {
        self.headers
            .get("content-range")
            .and_then(|v| v.to_str().ok())
    }

    fn preference_applied(&self) -> Option<&str> {
        self.headers
            .get("preference-applied")
            .and_then(|v| v.to_str().ok())
    }
}

async fn rest_get(
    server: &TestServer,
    token: &str,
    path: &str,
    prefer: Option<&str>,
) -> RestResponse {
    let client = reqwest::Client::new();
    let mut req = client
        .get(server.url(&format!("/rest/v1/{path}")))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(p) = prefer {
        req = req.header("Prefer", p);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp.text().await.unwrap();
    RestResponse {
        status,
        headers,
        body,
    }
}

async fn rest_post(
    server: &TestServer,
    token: &str,
    table: &str,
    prefer: Option<&str>,
    body: Value,
) -> RestResponse {
    let client = reqwest::Client::new();
    let mut req = client
        .post(server.url(&format!("/rest/v1/{table}")))
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

async fn rest_patch(
    server: &TestServer,
    token: &str,
    path: &str,
    prefer: Option<&str>,
    body: Value,
) -> RestResponse {
    let client = reqwest::Client::new();
    let mut req = client
        .patch(server.url(&format!("/rest/v1/{path}")))
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

async fn rest_delete(
    server: &TestServer,
    token: &str,
    path: &str,
    prefer: Option<&str>,
) -> RestResponse {
    let client = reqwest::Client::new();
    let mut req = client
        .delete(server.url(&format!("/rest/v1/{path}")))
        .header("Authorization", format!("Bearer {token}"));
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

fn rows(body: &Value) -> &Vec<Value> {
    body["rows"].as_array().unwrap()
}

/// `items(id INT, name TEXT)` with `n` rows (`id`/`name` = 1..=n), owned by a
/// fresh superuser `root`.
async fn setup_items_table(server: &TestServer, n: i64) -> String {
    let admin = valid_token();
    assert_eq!(
        sql(server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    let root = token_for("root");
    assert_eq!(
        sql(server, &root, "CREATE TABLE items (id INT, name TEXT)")
            .await
            .0,
        200
    );
    for i in 1..=n {
        let stmt = format!("INSERT INTO items (id, name) VALUES ({i}, 'item{i}')");
        assert_eq!(sql(server, &root, &stmt).await.0, 200, "seed row {i}");
    }
    root
}

// ── §1: `Prefer: count=exact` -> `Content-Range` ────────────────────────────

#[tokio::test]
async fn count_exact_with_limit_offset_sets_content_range() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 5).await;

    let resp = rest_get(
        &server,
        &root,
        "items?order=id.asc&limit=2",
        Some("count=exact"),
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body = resp.json();
    assert_eq!(rows(&body).len(), 2, "{body}");
    assert_eq!(resp.content_range(), Some("0-1/5"));
}

#[tokio::test]
async fn count_exact_with_offset_reports_correct_window() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 5).await;

    let resp = rest_get(
        &server,
        &root,
        "items?order=id.asc&limit=2&offset=2",
        Some("count=exact"),
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body = resp.json();
    assert_eq!(rows(&body).len(), 2, "{body}");
    assert_eq!(resp.content_range(), Some("2-3/5"));
}

#[tokio::test]
async fn count_exact_zero_rows_uses_star_window() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 5).await;

    let resp = rest_get(&server, &root, "items?id=eq.999", Some("count=exact")).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body = resp.json();
    assert_eq!(rows(&body).len(), 0, "{body}");
    assert_eq!(resp.content_range(), Some("*/0"));
}

/// Without `Prefer: count=exact`, no `Content-Range` header at all — the
/// backlog spec's "byte-identical response as today" contract.
#[tokio::test]
async fn no_prefer_header_omits_content_range() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 5).await;

    let resp = rest_get(&server, &root, "items?order=id.asc&limit=2", None).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert!(resp.content_range().is_none());
    assert!(resp.preference_applied().is_none());
    let body = resp.json();
    assert_eq!(rows(&body).len(), 2, "{body}");
}

/// Unknown `Prefer` tokens are ignored, not errors, and don't block a
/// recognized token in the same header value from applying.
#[tokio::test]
async fn unknown_prefer_tokens_are_ignored() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 5).await;

    let resp = rest_get(
        &server,
        &root,
        "items?order=id.asc&limit=2",
        Some("wat=nope, count=exact, resolution=merge-duplicates"),
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(resp.content_range(), Some("0-1/5"));
    // `Preference-Applied` echoes only recognized tokens.
    let applied = resp.preference_applied().unwrap();
    assert!(applied.contains("count=exact"), "{applied}");
    assert!(!applied.contains("wat=nope"), "{applied}");
}

/// Count respects RLS: a caller who can see only some of the table's rows
/// gets a total scoped to what they can see, with parity against a direct
/// `SELECT COUNT(*)` as that same principal.
#[tokio::test]
async fn count_exact_respects_rls() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    assert_eq!(
        sql(&server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    let root = token_for("root");

    assert_eq!(
        sql(&server, &root, "CREATE TABLE posts (id INT, owner TEXT)")
            .await
            .0,
        200
    );
    assert_eq!(sql(&server, &root, "CREATE USER alice").await.0, 200);
    assert_eq!(
        sql(&server, &root, "GRANT SELECT ON posts TO alice")
            .await
            .0,
        200
    );
    for i in 1..=10 {
        let owner = if i <= 3 { "alice" } else { "root" };
        let stmt = format!("INSERT INTO posts (id, owner) VALUES ({i}, '{owner}')");
        assert_eq!(sql(&server, &root, &stmt).await.0, 200, "seed row {i}");
    }
    let policy = sql(
        &server,
        &root,
        "CREATE POLICY p ON posts FOR SELECT USING (owner = current_user)",
    )
    .await;
    assert_eq!(policy.0, 200, "{:?}", policy.1);

    let alice = token_for("alice");

    // Parity target: a direct `SELECT COUNT(*)` as alice.
    let (status, sql_body) = sql(&server, &alice, "SELECT COUNT(*) FROM posts").await;
    assert_eq!(status, 200, "{sql_body}");
    let direct_count = sql_body["results"][0]["rows"][0][0].as_i64().unwrap();
    assert_eq!(direct_count, 3, "sanity: alice owns exactly 3 of 10 rows");

    // `/rest/v1` with `count=exact` must report the identical total — 3, not
    // 10 — and the returned rows themselves must also be RLS-scoped.
    let resp = rest_get(&server, &alice, "posts?select=id", Some("count=exact")).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body = resp.json();
    assert_eq!(rows(&body).len(), 3, "{body}");
    assert_eq!(resp.content_range(), Some("0-2/3"));
}

// ── §2: `Prefer: return=representation|minimal` on mutations ───────────────

#[tokio::test]
async fn post_no_prefer_keeps_current_count_body() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 0).await;

    let resp = rest_post(&server, &root, "items", None, json!({"id": 1, "name": "a"})).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body = resp.json();
    assert_eq!(body["type"], "inserted");
    assert_eq!(body["count"], 1);
    assert!(resp.preference_applied().is_none());
}

#[tokio::test]
async fn post_return_minimal_is_empty_201() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 0).await;

    let resp = rest_post(
        &server,
        &root,
        "items",
        Some("return=minimal"),
        json!({"id": 1, "name": "a"}),
    )
    .await;
    assert_eq!(resp.status, 201, "{}", resp.body);
    assert!(resp.body.is_empty(), "{:?}", resp.body);

    // Row was actually inserted despite the empty response.
    let check = rest_get(&server, &root, "items?id=eq.1", None).await;
    assert_eq!(rows(&check.json()).len(), 1);
}

#[tokio::test]
async fn post_return_representation_returns_inserted_rows() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 0).await;

    let resp = rest_post(
        &server,
        &root,
        "items",
        Some("return=representation"),
        json!([{"id": 1, "name": "a"}, {"id": 2, "name": "b"}]),
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body = resp.json();
    assert_eq!(body["type"], "rows");
    assert_eq!(rows(&body).len(), 2, "{body}");
}

#[tokio::test]
async fn patch_no_prefer_keeps_current_count_body() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 3).await;

    let resp = rest_patch(
        &server,
        &root,
        "items?id=eq.1",
        None,
        json!({"name": "updated"}),
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body = resp.json();
    assert_eq!(body["type"], "updated");
    assert_eq!(body["count"], 1);
}

#[tokio::test]
async fn patch_return_minimal_is_empty_204() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 3).await;

    let resp = rest_patch(
        &server,
        &root,
        "items?id=eq.2",
        Some("return=minimal"),
        json!({"name": "updated2"}),
    )
    .await;
    assert_eq!(resp.status, 204, "{}", resp.body);
    assert!(resp.body.is_empty(), "{:?}", resp.body);

    let check = rest_get(&server, &root, "items?id=eq.2&select=name", None).await;
    assert_eq!(rows(&check.json())[0][0], "updated2");
}

#[tokio::test]
async fn patch_return_representation_returns_updated_rows() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 3).await;

    let resp = rest_patch(
        &server,
        &root,
        "items?id=eq.1",
        Some("return=representation"),
        json!({"name": "updated"}),
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body = resp.json();
    assert_eq!(body["type"], "rows");
    let r = rows(&body);
    assert_eq!(r.len(), 1, "{body}");
    let name_idx = body["columns"]
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c == "name")
        .unwrap();
    assert_eq!(r[0][name_idx], "updated");
}

/// `return=representation` combined with the `in.(...)` multi-statement
/// expansion (this engine's `UPDATE` grammar has no native `IN`, see
/// `rest_resource.rs`'s module doc) must merge every expanded statement's
/// `RETURNING` rows into one logical result, not just the last one.
#[tokio::test]
async fn patch_in_filter_representation_merges_all_rows() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 5).await;

    let resp = rest_patch(
        &server,
        &root,
        "items?id=in.(1,3,5)",
        Some("return=representation"),
        json!({"name": "bulk"}),
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body = resp.json();
    assert_eq!(rows(&body).len(), 3, "{body}");
}

#[tokio::test]
async fn delete_no_prefer_keeps_current_count_body() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 3).await;

    let resp = rest_delete(&server, &root, "items?id=eq.1", None).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body = resp.json();
    assert_eq!(body["type"], "deleted");
    assert_eq!(body["count"], 1);
}

#[tokio::test]
async fn delete_return_minimal_is_empty_204() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 3).await;

    let resp = rest_delete(&server, &root, "items?id=eq.2", Some("return=minimal")).await;
    assert_eq!(resp.status, 204, "{}", resp.body);
    assert!(resp.body.is_empty(), "{:?}", resp.body);

    let check = rest_get(&server, &root, "items?id=eq.2", None).await;
    assert_eq!(rows(&check.json()).len(), 0);
}

#[tokio::test]
async fn delete_return_representation_returns_deleted_rows() {
    let server = TestServer::spawn().await;
    let root = setup_items_table(&server, 3).await;

    let resp = rest_delete(
        &server,
        &root,
        "items?id=eq.1",
        Some("return=representation"),
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let body = resp.json();
    assert_eq!(body["type"], "rows");
    assert_eq!(rows(&body).len(), 1, "{body}");

    let check = rest_get(&server, &root, "items?id=eq.1", None).await;
    assert_eq!(rows(&check.json()).len(), 0, "row should actually be gone");
}
