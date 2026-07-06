//! `POST /sql` (M5.d). **The central transaction-model proof**: a
//! multi-statement body where the last statement is deliberately invalid
//! must leave *zero* rows from the earlier statements visible afterward —
//! proving `handlers::finish`'s `Abort` really fires end-to-end over HTTP,
//! not just that `Engine::execute_sql` itself is atomic (already proven at
//! the Rust-API level since M1). The one deliberate exception, inherited
//! from M1 and not new to M5: `CREATE TABLE`'s catalog entry is *not*
//! rolled back (catalog DDL isn't transactional — see MEMORY.md's M1.c
//! design note) even though the row data is.

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

#[tokio::test]
async fn create_insert_select_round_trip() {
    let server = TestServer::spawn().await;

    let (status, _) = post_sql(&server, "CREATE TABLE t (id INT, name TEXT)").await;
    assert_eq!(status, 200);

    let (status, _) = post_sql(&server, "INSERT INTO t (id, name) VALUES (1, 'alice')").await;
    assert_eq!(status, 200);

    let (status, body) = post_sql(&server, "SELECT * FROM t").await;
    assert_eq!(status, 200);
    assert_eq!(
        body["results"][0]["rows"],
        serde_json::json!([[1, "alice"]])
    );
}

#[tokio::test]
async fn json_column_reparses_as_nested_json_not_a_string() {
    let server = TestServer::spawn().await;
    post_sql(&server, "CREATE TABLE t (id INT, data JSON)").await;
    post_sql(
        &server,
        r#"INSERT INTO t (id, data) VALUES (1, '{"status": "active"}')"#,
    )
    .await;

    let (_, body) = post_sql(&server, "SELECT * FROM t").await;
    let row = &body["results"][0]["rows"][0];
    // The JSON column must come back as a real nested object, not a
    // JSON-encoded string (`server::dto::literal_to_json`'s whole point).
    assert_eq!(row[1]["status"], "active");
}

#[tokio::test]
async fn multi_statement_body_is_atomic_failing_statement_rolls_back_row_data() {
    let server = TestServer::spawn().await;
    post_sql(&server, "CREATE TABLE t (id INT)").await;

    // The second statement's INSERT would succeed on its own, but the
    // third statement (a table that doesn't exist) fails — the whole
    // request is one transaction, so `finish` must abort it, and the
    // INSERT's row data must not be visible afterward.
    let (status, body) = post_sql(
        &server,
        "INSERT INTO t (id) VALUES (1); INSERT INTO nonexistent_table (id) VALUES (2)",
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(body["code"], "TABLE_NOT_FOUND");

    let (_, select_body) = post_sql(&server, "SELECT * FROM t").await;
    assert_eq!(
        select_body["results"][0]["rows"],
        serde_json::json!([]),
        "the INSERT from the aborted multi-statement request must not be visible"
    );
}
