//! REST enrichment R3 (deferred M8 routes: `POST /events/vacuum`,
//! `PUT /tables/{table}/rls`, `POST /admin/flush`) and R4 (`POST
//! /rows/batch`, SQL result cursors).

#[path = "server_common/mod.rs"]
mod server_common;

use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};
use server_common::{token_for, valid_token, TestServer};
use unidb::server::SessionConfig;

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn sql_as(server: &TestServer, token: &str, body: Value) -> (u16, Value) {
    let resp = client()
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn sql(server: &TestServer, sql_text: &str) -> (u16, Value) {
    sql_as(server, &valid_token(), json!({ "sql": sql_text })).await
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

// ── R3: POST /events/vacuum ──────────────────────────────────────────────

#[tokio::test]
async fn events_vacuum_reclaims_fully_acked_events() {
    let server = TestServer::spawn().await;
    let auth = format!("Bearer {}", valid_token());
    sql(&server, "CREATE TABLE orders (id INT)").await;

    // Enable capture, then generate two events.
    let resp = client()
        .post(server.url("/tables/orders/events"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    sql(&server, "INSERT INTO orders (id) VALUES (1)").await;
    sql(&server, "INSERT INTO orders (id) VALUES (2)").await;

    // With no consumer registered, nothing is reclaimable (the M4
    // slow-consumer durability contract: an event outlives vacuum until
    // every consumer has acked past it).
    let resp = client()
        .post(server.url("/events/vacuum"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["reclaimed"], 0, "no consumer has acked yet: {body}");

    // Ack far past both events, then vacuum reclaims exactly them.
    let resp = client()
        .post(server.url("/events/ack"))
        .header("Authorization", &auth)
        .json(&json!({"consumer": "worker", "up_to_seq": 1_000_000}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    let resp = client()
        .post(server.url("/events/vacuum"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["reclaimed"], 2, "{body}");
}

// ── R3: PUT /tables/{table}/rls ──────────────────────────────────────────

#[tokio::test]
async fn rls_policy_over_rest_filters_rows() {
    let server = TestServer::spawn().await;
    let auth = format!("Bearer {}", valid_token());
    sql(&server, "CREATE TABLE docs (tenant INT, body TEXT)").await;
    sql(
        &server,
        "INSERT INTO docs (tenant, body) VALUES (1, 'mine'); INSERT INTO docs (tenant, body) VALUES (2, 'theirs')",
    )
    .await;

    let resp = client()
        .put(server.url("/tables/docs/rls"))
        .header("Authorization", &auth)
        .json(&json!({"predicate": "tenant = 1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    // The policy is AND-rewritten into every query on the table.
    let (_, body) = sql(&server, "SELECT body FROM docs").await;
    assert_eq!(body["results"][0]["rows"], json!([["mine"]]));

    // A malformed or non-AND-only predicate is rejected.
    for bad in ["tenant = ", "tenant = 1 OR tenant = 2"] {
        let resp = client()
            .put(server.url("/tables/docs/rls"))
            .header("Authorization", &auth)
            .json(&json!({ "predicate": bad }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400, "predicate {bad:?} must 400");
    }

    // Unknown table → 404.
    let resp = client()
        .put(server.url("/tables/nope/rls"))
        .header("Authorization", &auth)
        .json(&json!({"predicate": "tenant = 1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn rls_and_flush_are_superuser_gated() {
    let server = TestServer::spawn().await;
    sql(&server, "CREATE TABLE t (id INT)").await;
    // Registering users ends open/bootstrap mode: "boss" is superuser,
    // "peon" is not (P6.e). `test-user` (the default token) is now a plain
    // named user with no privileges either.
    sql(&server, "CREATE USER boss SUPERUSER").await;
    sql_as(
        &server,
        &token_for("boss"),
        json!({"sql": "CREATE USER peon"}),
    )
    .await;

    // Non-superuser: both admin routes are 403.
    let resp = client()
        .put(server.url("/tables/t/rls"))
        .header("Authorization", format!("Bearer {}", token_for("peon")))
        .json(&json!({"predicate": "id = 1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    let resp = client()
        .post(server.url("/admin/flush"))
        .header("Authorization", format!("Bearer {}", token_for("peon")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);

    // Superuser: both succeed.
    let resp = client()
        .put(server.url("/tables/t/rls"))
        .header("Authorization", format!("Bearer {}", token_for("boss")))
        .json(&json!({"predicate": "id = 1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    let resp = client()
        .post(server.url("/admin/flush"))
        .header("Authorization", format!("Bearer {}", token_for("boss")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
}

#[tokio::test]
async fn admin_flush_succeeds_in_open_mode() {
    let server = TestServer::spawn().await;
    sql(&server, "CREATE TABLE t (id INT)").await;
    sql(&server, "INSERT INTO t (id) VALUES (1)").await;
    // Open/bootstrap mode (no registered users): any authenticated
    // principal is an effective superuser — backward compatible.
    let resp = client()
        .post(server.url("/admin/flush"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
}

// ── R4: POST /rows/batch ─────────────────────────────────────────────────

#[tokio::test]
async fn batch_insert_returns_ids_and_rows_round_trip() {
    let server = TestServer::spawn().await;
    let auth = format!("Bearer {}", valid_token());

    let payloads: Vec<Vec<u8>> = (0..5).map(|i| format!("row-{i}").into_bytes()).collect();
    let encoded: Vec<String> = payloads.iter().map(|p| b64(p)).collect();
    let resp = client()
        .post(server.url("/rows/batch"))
        .header("Authorization", &auth)
        .json(&json!({ "rows": encoded }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let body: Value = resp.json().await.unwrap();
    let ids = body["row_ids"].as_array().unwrap();
    assert_eq!(ids.len(), 5);

    for (i, id) in ids.iter().enumerate() {
        let (page, slot) = (
            id["page_id"].as_u64().unwrap(),
            id["slot"].as_u64().unwrap(),
        );
        let resp = client()
            .get(server.url(&format!("/rows/{page}/{slot}")))
            .header("Authorization", &auth)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.bytes().await.unwrap().as_ref(), payloads[i].as_slice());
    }
}

#[tokio::test]
async fn batch_insert_rejects_bad_input_without_inserting() {
    let server = TestServer::spawn().await;
    let auth = format!("Bearer {}", valid_token());

    // Malformed base64 mid-batch → 400, nothing inserted (validated before
    // any insert runs).
    let resp = client()
        .post(server.url("/rows/batch"))
        .header("Authorization", &auth)
        .json(&json!({ "rows": [b64(b"ok"), "!!!not-base64!!!", b64(b"ok2")] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BAD_BASE64");

    // Empty and oversized batches are rejected.
    let resp = client()
        .post(server.url("/rows/batch"))
        .header("Authorization", &auth)
        .json(&json!({ "rows": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    let too_many: Vec<String> = (0..10_001).map(|_| b64(b"x")).collect();
    let resp = client()
        .post(server.url("/rows/batch"))
        .header("Authorization", &auth)
        .json(&json!({ "rows": too_many }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BATCH_TOO_LARGE");
    // (Atomicity of the rejections is structural: every row is decoded and
    // bounds-checked before the first insert runs — see `post_rows_batch`.)
}

#[tokio::test]
async fn batch_insert_participates_in_sessions() {
    let server = TestServer::spawn().await;
    let auth = format!("Bearer {}", valid_token());

    // Begin a session, batch-insert inside it, roll back → rows gone.
    let resp = client()
        .post(server.url("/txn/begin"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let txn = resp.json::<Value>().await.unwrap()["txn_id"]
        .as_u64()
        .unwrap();

    let resp = client()
        .post(server.url("/rows/batch"))
        .header("Authorization", &auth)
        .header("X-Txn-Id", txn.to_string())
        .json(&json!({ "rows": [b64(b"a"), b64(b"b")] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let ids = resp.json::<Value>().await.unwrap()["row_ids"].clone();

    let resp = client()
        .post(server.url(&format!("/txn/{txn}/rollback")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let id = &ids[0];
    let (page, slot) = (
        id["page_id"].as_u64().unwrap(),
        id["slot"].as_u64().unwrap(),
    );
    let resp = client()
        .get(server.url(&format!("/rows/{page}/{slot}")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "rolled-back batch must vanish");
}

// ── R4: SQL result cursors ───────────────────────────────────────────────

async fn seed_numbers(server: &TestServer, n: usize) {
    sql(server, "CREATE TABLE nums (id INT)").await;
    // One multi-statement request keeps the seeding to a single commit.
    let stmts: Vec<String> = (0..n)
        .map(|i| format!("INSERT INTO nums (id) VALUES ({i})"))
        .collect();
    let (status, _) = sql(server, &stmts.join("; ")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn cursor_pages_a_result_to_exhaustion() {
    let server = TestServer::spawn().await;
    let auth = format!("Bearer {}", valid_token());
    seed_numbers(&server, 25).await;

    let (status, body) = sql_as(
        &server,
        &valid_token(),
        json!({"sql": "SELECT id FROM nums", "cursor": true}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let cursor_id = body["cursor_id"].as_u64().expect("cursor_id");
    assert_eq!(body["row_count"], 25);
    assert_eq!(body["columns"], json!(["id"]));

    let mut collected = 0usize;
    let mut pages = 0usize;
    loop {
        let resp = client()
            .get(server.url(&format!("/sql/cursor/{cursor_id}?limit=10")))
            .header("Authorization", &auth)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let page: Value = resp.json().await.unwrap();
        let rows = page["rows"].as_array().unwrap();
        assert!(rows.len() <= 10);
        collected += rows.len();
        pages += 1;
        if page["done"].as_bool().unwrap() {
            break;
        }
        assert!(pages < 10, "runaway pagination");
    }
    assert_eq!(collected, 25);
    assert_eq!(pages, 3, "25 rows at limit=10 → 10+10+5");

    // Exhausted cursor is gone.
    let resp = client()
        .get(server.url(&format!("/sql/cursor/{cursor_id}?limit=10")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "CURSOR_NOT_FOUND");
}

#[tokio::test]
async fn cursor_expires_on_idle_and_can_be_dropped_early() {
    let server = TestServer::spawn_with_sessions(SessionConfig {
        txn_idle_timeout: Duration::from_secs(60),
        cursor_idle_timeout: Duration::from_millis(300),
    })
    .await;
    let auth = format!("Bearer {}", valid_token());
    seed_numbers(&server, 3).await;

    // Expiry.
    let (_, body) = sql_as(
        &server,
        &valid_token(),
        json!({"sql": "SELECT id FROM nums", "cursor": true}),
    )
    .await;
    let expired_id = body["cursor_id"].as_u64().unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let resp = client()
        .get(server.url(&format!("/sql/cursor/{expired_id}")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404, "idle cursor must expire");

    // Early drop.
    let (_, body) = sql_as(
        &server,
        &valid_token(),
        json!({"sql": "SELECT id FROM nums", "cursor": true}),
    )
    .await;
    let id = body["cursor_id"].as_u64().unwrap();
    let resp = client()
        .delete(server.url(&format!("/sql/cursor/{id}")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    let resp = client()
        .get(server.url(&format!("/sql/cursor/{id}")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn cursor_is_principal_bound_and_requires_rows() {
    let server = TestServer::spawn().await;
    seed_numbers(&server, 2).await;

    // Alice's cursor is invisible to Bob.
    let (_, body) = sql_as(
        &server,
        &token_for("alice"),
        json!({"sql": "SELECT id FROM nums", "cursor": true}),
    )
    .await;
    let id = body["cursor_id"].as_u64().unwrap();
    let resp = client()
        .get(server.url(&format!("/sql/cursor/{id}")))
        .header("Authorization", format!("Bearer {}", token_for("bob")))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "CURSOR_FORBIDDEN");

    // Cursor mode on a non-rows statement is a 400.
    let (status, body) = sql_as(
        &server,
        &valid_token(),
        json!({"sql": "INSERT INTO nums (id) VALUES (99)", "cursor": true}),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["code"], "CURSOR_NOT_ROWS");
}
