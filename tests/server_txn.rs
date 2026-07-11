//! Transaction sessions over HTTP (REST enrichment R1) + one-shot isolation
//! selection (R2). Covers every hard design point from
//! `docs/backlog/rest_api_enrichment.md`: multi-statement atomicity,
//! repeatable-read snapshot stability across requests, idle auto-abort (and
//! horizon release), in-session serialization (`409 TXN_BUSY`), principal
//! binding (`403`), stale ids (`404`), plus the documented session error
//! semantics (DDL rejection, failed-statement auto-abort).

#[path = "server_common/mod.rs"]
mod server_common;

use std::time::Duration;

use serde_json::{json, Value};
use server_common::{token_for, valid_token, TestServer};
use unidb::server::SessionConfig;

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// POST /sql as `test-user`, optionally inside session `txn`.
async fn sql(server: &TestServer, txn: Option<u64>, body: Value) -> (u16, Value) {
    sql_as(server, &valid_token(), txn, body).await
}

async fn sql_as(server: &TestServer, token: &str, txn: Option<u64>, body: Value) -> (u16, Value) {
    let mut req = client()
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body);
    if let Some(txn_id) = txn {
        req = req.header("X-Txn-Id", txn_id.to_string());
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

/// POST /txn/begin; returns (status, body).
async fn begin(server: &TestServer, token: &str, body: Option<Value>) -> (u16, Value) {
    let mut req = client()
        .post(server.url("/txn/begin"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn begin_txn(server: &TestServer, isolation: Option<&str>) -> u64 {
    let body = isolation.map(|iso| json!({ "isolation": iso }));
    let (status, resp) = begin(server, &valid_token(), body).await;
    assert_eq!(status, 201, "begin failed: {resp}");
    resp["txn_id"].as_u64().expect("txn_id")
}

async fn txn_op(server: &TestServer, token: &str, txn_id: u64, op: &str) -> (u16, Value) {
    let resp = client()
        .post(server.url(&format!("/txn/{txn_id}/{op}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

async fn commit(server: &TestServer, txn_id: u64) -> (u16, Value) {
    txn_op(server, &valid_token(), txn_id, "commit").await
}

async fn rollback(server: &TestServer, txn_id: u64) -> (u16, Value) {
    txn_op(server, &valid_token(), txn_id, "rollback").await
}

fn rows_of(body: &Value) -> &Value {
    &body["results"][0]["rows"]
}

// ── R1: atomicity across requests ────────────────────────────────────────

#[tokio::test]
async fn session_multi_request_commit_is_atomic() {
    let server = TestServer::spawn().await;
    sql(&server, None, json!({"sql": "CREATE TABLE t (id INT)"})).await;

    let txn = begin_txn(&server, None).await;
    // Two separate requests inside the same transaction.
    let (s1, _) = sql(
        &server,
        Some(txn),
        json!({"sql": "INSERT INTO t (id) VALUES (1)"}),
    )
    .await;
    let (s2, _) = sql(
        &server,
        Some(txn),
        json!({"sql": "INSERT INTO t (id) VALUES (2)"}),
    )
    .await;
    assert_eq!((s1, s2), (200, 200));

    // Uncommitted work is invisible to other transactions…
    let (_, body) = sql(&server, None, json!({"sql": "SELECT * FROM t"})).await;
    assert_eq!(rows_of(&body).as_array().unwrap().len(), 0);
    // …but visible to the session itself.
    let (_, own) = sql(&server, Some(txn), json!({"sql": "SELECT * FROM t"})).await;
    assert_eq!(rows_of(&own).as_array().unwrap().len(), 2);

    let (status, body) = commit(&server, txn).await;
    assert_eq!(status, 200, "commit failed: {body}");
    assert_eq!(body["state"], "committed");

    let (_, body) = sql(&server, None, json!({"sql": "SELECT * FROM t"})).await;
    assert_eq!(rows_of(&body).as_array().unwrap().len(), 2);

    // The finished session id is gone.
    let (status, body) = commit(&server, txn).await;
    assert_eq!(status, 404, "double commit must be TXN_NOT_FOUND: {body}");
}

#[tokio::test]
async fn session_rollback_discards_all_requests() {
    let server = TestServer::spawn().await;
    sql(&server, None, json!({"sql": "CREATE TABLE t (id INT)"})).await;

    let txn = begin_txn(&server, None).await;
    sql(
        &server,
        Some(txn),
        json!({"sql": "INSERT INTO t (id) VALUES (1)"}),
    )
    .await;
    sql(
        &server,
        Some(txn),
        json!({"sql": "INSERT INTO t (id) VALUES (2)"}),
    )
    .await;

    let (status, body) = rollback(&server, txn).await;
    assert_eq!(status, 200);
    assert_eq!(body["state"], "rolled_back");

    let (_, body) = sql(&server, None, json!({"sql": "SELECT * FROM t"})).await;
    assert_eq!(
        rows_of(&body).as_array().unwrap().len(),
        0,
        "all-or-nothing"
    );
}

// ── R1: snapshot isolation across requests ──────────────────────────────

#[tokio::test]
async fn repeatable_read_session_sees_stable_snapshot_across_requests() {
    let server = TestServer::spawn().await;
    sql(&server, None, json!({"sql": "CREATE TABLE t (id INT)"})).await;
    sql(
        &server,
        None,
        json!({"sql": "INSERT INTO t (id) VALUES (1)"}),
    )
    .await;

    let txn = begin_txn(&server, Some("repeatable_read")).await;
    let (_, before) = sql(&server, Some(txn), json!({"sql": "SELECT * FROM t"})).await;
    assert_eq!(rows_of(&before).as_array().unwrap().len(), 1);

    // A concurrent one-shot write commits between the session's requests.
    let (s, _) = sql(
        &server,
        None,
        json!({"sql": "INSERT INTO t (id) VALUES (2)"}),
    )
    .await;
    assert_eq!(s, 200);

    // The RR session still sees its BEGIN-time snapshot…
    let (_, after) = sql(&server, Some(txn), json!({"sql": "SELECT * FROM t"})).await;
    assert_eq!(
        rows_of(&after).as_array().unwrap().len(),
        1,
        "repeatable_read must hold one stable snapshot across HTTP requests"
    );
    commit(&server, txn).await;

    // …and a fresh statement sees both rows.
    let (_, fresh) = sql(&server, None, json!({"sql": "SELECT * FROM t"})).await;
    assert_eq!(rows_of(&fresh).as_array().unwrap().len(), 2);
}

// ── R1: idle reaper ──────────────────────────────────────────────────────

#[tokio::test]
async fn idle_session_is_auto_aborted_and_releases_the_horizon() {
    let server = TestServer::spawn_with_sessions(SessionConfig {
        txn_idle_timeout: Duration::from_millis(300),
        cursor_idle_timeout: Duration::from_secs(60),
    })
    .await;
    sql(&server, None, json!({"sql": "CREATE TABLE t (id INT)"})).await;

    let txn = begin_txn(&server, None).await;
    let (s, _) = sql(
        &server,
        Some(txn),
        json!({"sql": "INSERT INTO t (id) VALUES (1)"}),
    )
    .await;
    assert_eq!(s, 200);

    // Abandon the session past its idle deadline; the reaper must abort it.
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let (status, body) = sql(&server, Some(txn), json!({"sql": "SELECT * FROM t"})).await;
    assert_eq!(status, 404, "expired session must be TXN_NOT_FOUND: {body}");
    assert_eq!(body["code"], "TXN_NOT_FOUND");

    // The engine transaction is really gone (horizon un-pinned, locks freed):
    // no active transactions remain and the uncommitted row never lands.
    let resp = client()
        .get(server.url("/stats"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    let stats: Value = resp.json().await.unwrap();
    assert_eq!(stats["active_transactions"], 0, "stats: {stats}");
    assert_eq!(stats["open_txn_sessions"], 0);

    let (_, rows) = sql(&server, None, json!({"sql": "SELECT * FROM t"})).await;
    assert_eq!(rows_of(&rows).as_array().unwrap().len(), 0);
}

// ── R1: in-session serialization ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_request_on_busy_session_is_409_txn_busy() {
    let server = TestServer::spawn().await;
    sql(&server, None, json!({"sql": "CREATE TABLE t (id INT)"})).await;

    // Occupy the session with one long multi-statement request (a few
    // thousand INSERT statements comfortably outlasts the probe below in a
    // debug build), then hit the same session concurrently.
    let txn = begin_txn(&server, None).await;
    let long_body: String = (0..3000)
        .map(|i| format!("INSERT INTO t (id) VALUES ({i})"))
        .collect::<Vec<_>>()
        .join("; ");
    let server_url = server.url("/sql");
    let in_flight = tokio::spawn(async move {
        let resp = client()
            .post(server_url)
            .header("Authorization", format!("Bearer {}", valid_token()))
            .header("X-Txn-Id", txn.to_string())
            .json(&json!({ "sql": long_body }))
            .send()
            .await
            .unwrap();
        resp.status().as_u16()
    });

    // Wait until the long statement is inside the engine, then probe the
    // busy session: it must conflict rather than corrupt the transaction.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (status, body) = sql(&server, Some(txn), json!({"sql": "SELECT * FROM t"})).await;
    assert_eq!(status, 409, "busy session must conflict: {body}");
    assert_eq!(body["code"], "TXN_BUSY");

    // The long request finishes fine, the session survives the 409, and the
    // whole batch commits atomically.
    assert_eq!(in_flight.await.unwrap(), 200);
    let (s, _) = commit(&server, txn).await;
    assert_eq!(s, 200);
    let (_, rows) = sql(&server, None, json!({"sql": "SELECT * FROM t"})).await;
    assert_eq!(rows_of(&rows).as_array().unwrap().len(), 3000);
}

// ── R1: principal binding + stale ids ────────────────────────────────────

#[tokio::test]
async fn session_is_bound_to_its_principal() {
    let server = TestServer::spawn().await;
    let (status, body) = begin(&server, &token_for("alice"), None).await;
    assert_eq!(status, 201);
    let txn = body["txn_id"].as_u64().unwrap();

    // A different (validly authenticated) principal cannot use it…
    let (status, body) = sql_as(
        &server,
        &token_for("bob"),
        Some(txn),
        json!({"sql": "SELECT 1"}),
    )
    .await;
    assert_eq!(status, 403, "cross-principal use must be forbidden: {body}");
    assert_eq!(body["code"], "TXN_FORBIDDEN");
    // …nor commit it.
    let (status, _) = txn_op(&server, &token_for("bob"), txn, "commit").await;
    assert_eq!(status, 403);

    // The owner still can.
    let (status, _) = txn_op(&server, &token_for("alice"), txn, "rollback").await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn stale_or_unknown_txn_id_is_404() {
    let server = TestServer::spawn().await;
    let (status, body) = sql(&server, Some(999_999), json!({"sql": "SELECT 1"})).await;
    assert_eq!(status, 404);
    assert_eq!(body["code"], "TXN_NOT_FOUND");

    let (status, _) = commit(&server, 999_999).await;
    assert_eq!(status, 404);

    // Malformed header is a 400, not a 404.
    let resp = client()
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .header("X-Txn-Id", "not-a-number")
        .json(&json!({"sql": "SELECT 1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "BAD_TXN_ID");
}

// ── R1: session error semantics ──────────────────────────────────────────

#[tokio::test]
async fn ddl_in_session_is_rejected_and_session_survives() {
    let server = TestServer::spawn().await;
    sql(&server, None, json!({"sql": "CREATE TABLE t (id INT)"})).await;

    let txn = begin_txn(&server, None).await;
    for ddl in [
        "CREATE TABLE u (id INT)",
        "DROP TABLE t",
        "TRUNCATE TABLE t",
        "CREATE USER eve SUPERUSER",
    ] {
        let (status, body) = sql(&server, Some(txn), json!({"sql": ddl})).await;
        assert_eq!(status, 400, "DDL must be rejected in a session: {ddl}");
        assert_eq!(body["code"], "DDL_IN_SESSION", "{ddl}: {body}");
    }
    // The rejection executed nothing — the session is still usable.
    let (s, _) = sql(
        &server,
        Some(txn),
        json!({"sql": "INSERT INTO t (id) VALUES (1)"}),
    )
    .await;
    assert_eq!(s, 200);
    let (s, _) = commit(&server, txn).await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn failed_statement_aborts_the_session() {
    let server = TestServer::spawn().await;
    sql(&server, None, json!({"sql": "CREATE TABLE t (id INT)"})).await;

    let txn = begin_txn(&server, None).await;
    sql(
        &server,
        Some(txn),
        json!({"sql": "INSERT INTO t (id) VALUES (1)"}),
    )
    .await;
    // A failing statement may have left partial effects → the transaction
    // is aborted and the session destroyed (Postgres-without-savepoints).
    let (status, _) = sql(
        &server,
        Some(txn),
        json!({"sql": "INSERT INTO missing_table (id) VALUES (1)"}),
    )
    .await;
    assert_eq!(status, 404, "the statement error itself");

    let (status, body) = commit(&server, txn).await;
    assert_eq!(
        status, 404,
        "session must be gone after a failed statement: {body}"
    );
    assert_eq!(body["code"], "TXN_NOT_FOUND");

    // Nothing from the aborted session is visible.
    let (_, rows) = sql(&server, None, json!({"sql": "SELECT * FROM t"})).await;
    assert_eq!(rows_of(&rows).as_array().unwrap().len(), 0);
}

/// Pure reads are lenient: a 404 probe for a deleted row must NOT destroy
/// the session (unlike a failed mutation).
#[tokio::test]
async fn read_miss_leaves_session_open() {
    let server = TestServer::spawn().await;
    let auth = format!("Bearer {}", valid_token());

    // Materialize a row, then delete it — a probe for it is a clean
    // no-visible-version 404 (an unallocated page would be a 500 instead).
    let resp = client()
        .post(server.url("/rows"))
        .header("Authorization", &auth)
        .body("ephemeral")
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    let (page, slot) = (
        body["row_id"]["page_id"].as_u64().unwrap(),
        body["row_id"]["slot"].as_u64().unwrap(),
    );
    let resp = client()
        .delete(server.url(&format!("/rows/{page}/{slot}")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    let txn = begin_txn(&server, None).await;
    let resp = client()
        .get(server.url(&format!("/rows/{page}/{slot}")))
        .header("Authorization", &auth)
        .header("X-Txn-Id", txn.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    // Session still alive and committable.
    let (status, _) = commit(&server, txn).await;
    assert_eq!(status, 200);
}

/// Raw-CRUD + session end-to-end: writes made via /rows under a session are
/// invisible until commit and readable inside it.
#[tokio::test]
async fn raw_rows_participate_in_sessions() {
    let server = TestServer::spawn().await;
    let txn = begin_txn(&server, None).await;
    let auth = format!("Bearer {}", valid_token());

    let resp = client()
        .post(server.url("/rows"))
        .header("Authorization", &auth)
        .header("X-Txn-Id", txn.to_string())
        .body("session-payload")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let body: Value = resp.json().await.unwrap();
    let (page, slot) = (
        body["row_id"]["page_id"].as_u64().unwrap(),
        body["row_id"]["slot"].as_u64().unwrap(),
    );

    // Outside the session: not visible (one-shot read path).
    let resp = client()
        .get(server.url(&format!("/rows/{page}/{slot}")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "uncommitted row must be invisible"
    );

    // Inside the session: visible.
    let resp = client()
        .get(server.url(&format!("/rows/{page}/{slot}")))
        .header("Authorization", &auth)
        .header("X-Txn-Id", txn.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"session-payload");

    commit(&server, txn).await;
    let resp = client()
        .get(server.url(&format!("/rows/{page}/{slot}")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "committed row must be visible");
}

// ── R2: one-shot isolation selection ─────────────────────────────────────

#[tokio::test]
async fn one_shot_isolation_field_is_accepted_and_session_isolation_is_fixed() {
    let server = TestServer::spawn().await;
    sql(&server, None, json!({"sql": "CREATE TABLE t (id INT)"})).await;

    for iso in ["read_committed", "repeatable_read", "serializable"] {
        let (status, body) = sql(
            &server,
            None,
            json!({"sql": "SELECT * FROM t", "isolation": iso}),
        )
        .await;
        assert_eq!(status, 200, "one-shot at {iso}: {body}");
    }

    // Unknown level is a 4xx rejection (axum body-deserialization error),
    // not a silent fallback to READ COMMITTED. Raw request — the rejection
    // body is not JSON.
    let resp = client()
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .json(&json!({"sql": "SELECT * FROM t", "isolation": "chaos"}))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert!(
        (400..500).contains(&status),
        "unknown isolation must not silently succeed: {status}"
    );

    // Inside a session the isolation field is rejected — it was fixed at begin.
    let txn = begin_txn(&server, Some("serializable")).await;
    let (status, body) = sql(
        &server,
        Some(txn),
        json!({"sql": "SELECT * FROM t", "isolation": "read_committed"}),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body["code"], "ISOLATION_IN_SESSION");
    rollback(&server, txn).await;
}

/// R2 acceptance: a serializable write-skew attempt is rejected with
/// `409 SERIALIZATION_FAILURE`. One side is a serializable *session*, the
/// other a one-shot serializable statement (proving the one-shot `isolation`
/// field really participates in SSI — at RC/RR the skew would commit).
#[tokio::test]
async fn serializable_write_skew_is_rejected_409() {
    let server = TestServer::spawn().await;
    // The canonical on-call write-skew (mirrors the engine's own P1.d test).
    sql(
        &server,
        None,
        json!({"sql": "CREATE TABLE doctors (id INT, on_call INT)"}),
    )
    .await;
    sql(
        &server,
        None,
        json!({"sql": "INSERT INTO doctors (id, on_call) VALUES (1, 1); INSERT INTO doctors (id, on_call) VALUES (2, 1)"}),
    )
    .await;

    // Session S (serializable): reads the on-call set, takes doctor 1 off.
    let txn = begin_txn(&server, Some("serializable")).await;
    let (s, _) = sql(
        &server,
        Some(txn),
        json!({"sql": "SELECT id FROM doctors WHERE on_call = 1"}),
    )
    .await;
    assert_eq!(s, 200);
    let (s_upd, upd_body) = sql(
        &server,
        Some(txn),
        json!({"sql": "UPDATE doctors SET on_call = 0 WHERE id = 1"}),
    )
    .await;

    // One-shot serializable transaction (two statements, atomically one
    // txn): reads the same on-call set — including the row S superseded —
    // and takes the *other* doctor off. Classic row-disjoint skew.
    let (oneshot_status, oneshot_body) = sql(
        &server,
        None,
        json!({
            "sql": "SELECT id FROM doctors WHERE on_call = 1; UPDATE doctors SET on_call = 0 WHERE id = 2",
            "isolation": "serializable"
        }),
    )
    .await;

    let (commit_status, commit_body) = if s_upd == 200 {
        commit(&server, txn).await
    } else {
        (409, upd_body.clone())
    };

    // SSI must refuse to serialize the skew: at least one side fails with
    // SERIALIZATION_FAILURE (a pivot pair may occasionally both abort —
    // sound, documented as over-conservative).
    let failures: Vec<&Value> = [
        (oneshot_status, &oneshot_body),
        (commit_status, &commit_body),
    ]
    .into_iter()
    .filter(|(s, _)| *s == 409)
    .map(|(_, b)| b)
    .collect();
    assert!(
        !failures.is_empty(),
        "write-skew must not serialize: one-shot {oneshot_status} {oneshot_body} / commit {commit_status} {commit_body}"
    );
    for body in failures {
        assert_eq!(body["code"], "SERIALIZATION_FAILURE", "{body}");
    }
}

// ── begin response shape ─────────────────────────────────────────────────

#[tokio::test]
async fn begin_response_carries_isolation_and_expiry() {
    let server = TestServer::spawn().await;
    let (status, body) = begin(
        &server,
        &valid_token(),
        Some(json!({"isolation": "repeatable_read"})),
    )
    .await;
    assert_eq!(status, 201);
    assert!(body["txn_id"].is_u64());
    assert_eq!(body["txn_id"], body["xid"], "compat alias");
    assert_eq!(body["isolation"], "repeatable_read");
    assert!(body["expires_at"].is_string());
    assert!(body["idle_timeout_secs"].is_u64());
    rollback(&server, body["txn_id"].as_u64().unwrap()).await;

    // Malformed body is a hard 400, not a silent default.
    let resp = client()
        .post(server.url("/txn/begin"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .header("Content-Type", "application/json")
        .body("{\"isolation\": \"chaos\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}
