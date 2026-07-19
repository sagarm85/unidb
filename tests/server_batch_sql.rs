//! Integration tests for `POST /batch-sql` (item 99).
//!
//! Each statement in a batch is an independent one-shot auto-commit — there
//! is no shared transaction across the batch.  The response is always
//! `200 OK`; per-statement failures are reported inside the payload.
//!
//! Test matrix:
//!  1. Batch of 3 successful SELECTs — 3 results, 3 null errors.
//!  2. One failing statement with `stop_on_error: false` — result/error
//!     arrays have the right null pattern, successful stmts still commit.
//!  3. `stop_on_error: true` — stops at first error, remaining slots are
//!     `"skipped"`.
//!  4. 257 statements → `400 BATCH_TOO_LARGE`.
//!  5. Mixed read (SELECT) and write (INSERT) statements.
//!  6. Empty batch — succeeds with empty arrays.

#[path = "server_common/mod.rs"]
mod server_common;

use serde_json::Value;
use server_common::{valid_token, TestServer};

async fn post_sql(server: &TestServer, sql: &str) -> (u16, Value) {
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .json(&serde_json::json!({ "sql": sql }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn post_batch_sql(server: &TestServer, payload: serde_json::Value) -> (u16, Value) {
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url("/batch-sql"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .json(&payload)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

// ── 1. Three successful SELECTs ───────────────────────────────────────────

#[tokio::test]
async fn batch_three_selects_returns_three_results() {
    let server = TestServer::spawn().await;
    post_sql(&server, "CREATE TABLE t (id INT, name TEXT)").await;
    post_sql(&server, "INSERT INTO t (id, name) VALUES (1, 'alice')").await;
    post_sql(&server, "INSERT INTO t (id, name) VALUES (2, 'bob')").await;

    let (status, body) = post_batch_sql(
        &server,
        serde_json::json!({
            "statements": [
                "SELECT COUNT(*) FROM t",
                "SELECT * FROM t WHERE id = 1",
                "SELECT * FROM t WHERE id = 2"
            ]
        }),
    )
    .await;

    assert_eq!(status, 200, "batch-sql must return 200 OK: {body}");

    let results = &body["results"];
    let errors = &body["errors"];

    assert_eq!(results.as_array().unwrap().len(), 3);
    assert_eq!(errors.as_array().unwrap().len(), 3);

    // All errors must be null (no failures).
    for i in 0..3 {
        assert!(
            errors[i].is_null(),
            "errors[{i}] should be null: {}",
            errors[i]
        );
    }

    // First result: COUNT(*) = 2
    assert_eq!(results[0]["type"], "rows");
    assert_eq!(results[0]["rows"][0][0], 2);

    // Second result: row for id=1
    assert_eq!(results[1]["type"], "rows");
    assert_eq!(results[1]["rows"][0][0], 1);
    assert_eq!(results[1]["rows"][0][1], "alice");

    // Third result: row for id=2
    assert_eq!(results[2]["type"], "rows");
    assert_eq!(results[2]["rows"][0][0], 2);
    assert_eq!(results[2]["rows"][0][1], "bob");
}

// ── 2. One failing stmt, stop_on_error: false ────────────────────────────

#[tokio::test]
async fn batch_one_failure_stop_on_error_false_continues() {
    let server = TestServer::spawn().await;
    post_sql(&server, "CREATE TABLE t (id INT)").await;
    post_sql(&server, "INSERT INTO t (id) VALUES (10)").await;

    let (status, body) = post_batch_sql(
        &server,
        serde_json::json!({
            "statements": [
                "SELECT * FROM t",
                "SELECT * FROM nonexistent_table",
                "SELECT COUNT(*) FROM t"
            ],
            "stop_on_error": false
        }),
    )
    .await;

    assert_eq!(status, 200, "batch always returns 200: {body}");

    let results = &body["results"];
    let errors = &body["errors"];
    assert_eq!(results.as_array().unwrap().len(), 3);
    assert_eq!(errors.as_array().unwrap().len(), 3);

    // Slot 0: successful SELECT
    assert!(!results[0].is_null(), "slot 0 must have a result");
    assert!(errors[0].is_null(), "slot 0 must have no error");

    // Slot 1: failed SELECT — null result, non-null error
    assert!(
        results[1].is_null(),
        "slot 1 result must be null on failure"
    );
    assert!(!errors[1].is_null(), "slot 1 must have an error string");
    let err_str = errors[1].as_str().unwrap();
    assert!(
        !err_str.is_empty(),
        "error string must be non-empty: {err_str}"
    );

    // Slot 2: successful even though slot 1 failed (stop_on_error: false)
    assert!(!results[2].is_null(), "slot 2 must have a result");
    assert!(errors[2].is_null(), "slot 2 must have no error");
    assert_eq!(results[2]["type"], "rows");
    assert_eq!(results[2]["rows"][0][0], 1, "COUNT(*) should be 1");
}

// ── 3. stop_on_error: true stops at first failure ────────────────────────

#[tokio::test]
async fn batch_stop_on_error_true_skips_remaining() {
    let server = TestServer::spawn().await;
    post_sql(&server, "CREATE TABLE t (id INT)").await;

    let (status, body) = post_batch_sql(
        &server,
        serde_json::json!({
            "statements": [
                "SELECT COUNT(*) FROM t",
                "SELECT * FROM does_not_exist",
                "SELECT COUNT(*) FROM t"
            ],
            "stop_on_error": true
        }),
    )
    .await;

    assert_eq!(status, 200, "batch-sql must return 200: {body}");

    let results = &body["results"];
    let errors = &body["errors"];

    // Slot 0: succeeded before the error.
    assert!(!results[0].is_null(), "slot 0 must have a result");
    assert!(errors[0].is_null());

    // Slot 1: the failing statement.
    assert!(results[1].is_null());
    assert!(!errors[1].is_null());

    // Slot 2: skipped because stop_on_error: true.
    assert!(results[2].is_null(), "slot 2 must be null (skipped)");
    assert_eq!(
        errors[2].as_str(),
        Some("skipped"),
        "slot 2 error must be 'skipped'"
    );
}

// ── 4. 257 statements → 400 BATCH_TOO_LARGE ─────────────────────────────

#[tokio::test]
async fn batch_too_large_returns_400() {
    let server = TestServer::spawn().await;

    let stmts: Vec<String> = (0..257).map(|_| "SELECT 1".to_string()).collect();
    let (status, body) = post_batch_sql(&server, serde_json::json!({ "statements": stmts })).await;

    assert_eq!(status, 400, "expected 400, got {status}: {body}");
    assert_eq!(
        body["code"], "BATCH_TOO_LARGE",
        "expected BATCH_TOO_LARGE code: {body}"
    );
}

// ── 5. Mixed read/write statements all commit independently ───────────────

#[tokio::test]
async fn batch_mixed_read_write_each_auto_commits() {
    let server = TestServer::spawn().await;
    post_sql(&server, "CREATE TABLE t (id INT)").await;

    let (status, body) = post_batch_sql(
        &server,
        serde_json::json!({
            "statements": [
                "INSERT INTO t (id) VALUES (1)",
                "INSERT INTO t (id) VALUES (2)",
                "SELECT COUNT(*) FROM t"
            ]
        }),
    )
    .await;

    assert_eq!(status, 200, "batch-sql must return 200: {body}");

    let results = &body["results"];
    let errors = &body["errors"];

    // Both inserts committed.
    assert_eq!(results[0]["type"], "inserted");
    assert_eq!(results[0]["count"], 1);
    assert!(errors[0].is_null());

    assert_eq!(results[1]["type"], "inserted");
    assert_eq!(results[1]["count"], 1);
    assert!(errors[1].is_null());

    // SELECT sees both committed rows.
    assert_eq!(results[2]["type"], "rows");
    assert_eq!(results[2]["rows"][0][0], 2, "COUNT(*) should be 2");
    assert!(errors[2].is_null());
}

// ── 6. Empty batch → 200 with empty arrays ───────────────────────────────

#[tokio::test]
async fn batch_empty_statements_returns_empty_arrays() {
    let server = TestServer::spawn().await;

    let (status, body) = post_batch_sql(&server, serde_json::json!({ "statements": [] })).await;

    assert_eq!(status, 200, "empty batch must succeed: {body}");
    assert_eq!(body["results"], serde_json::json!([]));
    assert_eq!(body["errors"], serde_json::json!([]));
}
