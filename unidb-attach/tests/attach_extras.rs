// Index, event, and checkpoint routes via AttachClient (M8.c).

#[path = "attach_common/mod.rs"]
mod attach_common;

use attach_common::{valid_token, TestServer};
use unidb_attach::{AttachClient, IndexKind};

fn client(server: &TestServer) -> AttachClient {
    AttachClient::new(&server.base_url, valid_token()).unwrap()
}

// ── Secondary indexes ─────────────────────────────────────────────────────────

#[test]
fn set_column_index_api_call_succeeds() {
    let server = TestServer::spawn();
    let c = client(&server);

    c.execute_sql("CREATE TABLE t (id INT, embedding VECTOR(3))")
        .unwrap();

    // `set_column_index` (`POST /indexes`) marks the column in the catalog.
    // It does NOT notify the background index worker, so `index_status`
    // remains `None` until the engine is reopened — this is by design (see
    // M2.b design note in MEMORY.md).  Use SQL `CREATE INDEX` when you need
    // the worker to start immediately.
    c.set_column_index("t", "embedding", Some(IndexKind::Hnsw))
        .unwrap();

    // Clearing the index also succeeds.
    c.set_column_index("t", "embedding", None).unwrap();
}

#[test]
fn index_status_none_before_create_index() {
    let server = TestServer::spawn();
    let c = client(&server);

    c.execute_sql("CREATE TABLE t (id INT, embedding VECTOR(3))")
        .unwrap();

    let status = c.index_status("t", "embedding").unwrap();
    assert!(
        status.is_none(),
        "no index_status before CREATE INDEX: got {status:?}"
    );
}

#[test]
fn create_index_via_sql_makes_status_some() {
    let server = TestServer::spawn();
    let c = client(&server);

    c.execute_sql("CREATE TABLE t (id INT, embedding VECTOR(3))")
        .unwrap();

    // `CREATE INDEX ... USING HNSW` goes through the SQL executor which
    // sends `MarkReady` to the background worker.  For an empty table the
    // worker transitions immediately, but message delivery is asynchronous
    // (bounded-channel, separate goroutine), so we poll briefly.
    c.execute_sql("CREATE INDEX idx ON t USING HNSW (embedding)")
        .unwrap();

    let status = (0..50).find_map(|_| {
        let s = c.index_status("t", "embedding").unwrap();
        if s.is_some() {
            Some(s)
        } else {
            std::thread::sleep(std::time::Duration::from_millis(10));
            None
        }
    });
    assert!(
        status.is_some(),
        "index_status must become Some after CREATE INDEX"
    );
}

// ── Events ────────────────────────────────────────────────────────────────────

#[test]
fn enable_events_and_ack_succeeds() {
    let server = TestServer::spawn();
    let c = client(&server);

    c.execute_sql("CREATE TABLE t (id INT)").unwrap();
    c.enable_events("t").unwrap();
    c.execute_sql("INSERT INTO t (id) VALUES (1)").unwrap();

    // Ack seq=1 — no SSE client needed; the server returns 204 and
    // `ack_events` returns `Ok(())`.
    c.ack_events("consumer1", 1).unwrap();
}

#[test]
fn enable_events_idempotent() {
    let server = TestServer::spawn();
    let c = client(&server);

    c.execute_sql("CREATE TABLE t (id INT)").unwrap();
    c.enable_events("t").unwrap();
    // Calling enable_events a second time on the same table must not error.
    c.enable_events("t").unwrap();
}

// ── Checkpoint ────────────────────────────────────────────────────────────────

#[test]
fn checkpoint_succeeds_after_writes() {
    let server = TestServer::spawn();
    let c = client(&server);

    c.execute_sql("CREATE TABLE t (id INT)").unwrap();
    c.execute_sql("INSERT INTO t (id) VALUES (42)").unwrap();

    // Checkpoint flushes dirty pages, writes a checkpoint record, and
    // truncates the WAL — must succeed without error.
    c.checkpoint().unwrap();

    // Data committed before the checkpoint must still be readable.
    let r = c.execute_sql("SELECT * FROM t").unwrap();
    let unidb_attach::ExecResult::Rows(rows) = &r[0] else {
        panic!("expected Rows");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], serde_json::json!(42));
}
