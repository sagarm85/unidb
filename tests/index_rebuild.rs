// Secondary-index rebuild/staleness correctness tests (M2.d).
//
// Separate from `tests/crash/main.rs`'s durability-focused P-numbering
// because this tests *derived, intentionally-not-durable* state (the
// background worker's `VectorIndex`/`InvertedIndex`), not WAL-durable
// state. Losing a secondary index on crash is expected and fine — it
// rebuilds on next open — so there's no new crash-injection point here,
// just correctness of that rebuild and of querying while it's in progress.

use tempfile::tempdir;
use unidb::index_worker::IndexStatus;
use unidb::sql::executor::ExecResult as SqlResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

fn wait_for_ready(engine: &Engine, table: &str, column: &str) {
    let start = std::time::Instant::now();
    loop {
        if engine.index_status(table, column) == Some(IndexStatus::Ready) {
            return;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!(
                "index status for {table}.{column} never reached Ready, last seen {:?}",
                engine.index_status(table, column)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn engine_restart_rebuilds_vector_index_and_near_still_works() {
    let dir = tempdir().unwrap();
    {
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, embedding) VALUES (1, [0.0, 0.0])")
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, embedding) VALUES (2, [50.0, 50.0])",
            )
            .unwrap();
        engine.commit(xid).unwrap();
        engine.flush().unwrap();
    }

    // Fresh process-equivalent open: the in-memory worker/index from the
    // first `Engine` is gone. Only the catalog's `index: Some(Hnsw)` flag
    // and the heap's committed rows survived — rebuild-on-open must
    // reconstruct the index from those alone.
    let mut engine2 = Engine::open(dir.path(), 0).unwrap();
    wait_for_ready(&engine2, "t", "embedding");

    let xid = engine2.begin().unwrap();
    let results = engine2
        .execute_sql(xid, "SELECT id FROM t WHERE NEAR(embedding, [0.0, 0.0], 1)")
        .unwrap();
    match &results[0] {
        SqlResult::Rows(rows) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Literal::Int(1));
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// P3.b made full-text durable: reopening does NOT rebuild it from the heap —
/// it reads the on-disk B+tree — and `Engine::search_fulltext` (the new read
/// path) works after restart with no `Ready` wait.
#[test]
fn fulltext_index_is_durable_and_searchable_after_reopen() {
    let dir = tempdir().unwrap();
    {
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, body TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING FULLTEXT (body)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, body) VALUES (1, 'rust engine')")
            .unwrap();
        engine.commit(xid).unwrap();
        engine.checkpoint().unwrap();
    }

    // Reopen: no heap rescan to rebuild the index, no `Ready` to wait on.
    let mut engine2 = Engine::open(dir.path(), 0).unwrap();
    let xid = engine2.begin().unwrap();
    let hits = engine2.search_fulltext(xid, "t", "body", "rust").unwrap();
    assert_eq!(
        hits.len(),
        1,
        "durable full-text must find the row after reopen"
    );
    assert!(engine2
        .search_fulltext(xid, "t", "body", "absent")
        .unwrap()
        .is_empty());
    engine2.commit(xid).unwrap();
}

/// P3.a changed the B-Tree from a rebuilt-on-open in-memory index to a
/// **durable** one: reopening does NOT rescan the heap to reconstruct it — it
/// reads the tree straight from its meta page — and there is no `Ready` status
/// to wait on. This test confirms an index-assisted query works after reopen
/// with no rebuild (was `engine_restart_rebuilds_btree_index_and_select_...`).
#[test]
fn engine_restart_btree_index_is_durable_no_rebuild() {
    let dir = tempdir().unwrap();
    {
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING BTREE (id)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (1, 'alice')")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (2, 'bob')")
            .unwrap();
        engine.commit(xid).unwrap();
        engine.flush().unwrap();
    }

    // Fresh process-equivalent open: the durable tree is read from its meta
    // page — no heap rescan, no `Ready` wait — and the query still works.
    let mut engine2 = Engine::open(dir.path(), 0).unwrap();
    let xid = engine2.begin().unwrap();
    let results = engine2
        .execute_sql(xid, "SELECT name FROM t WHERE id = 2")
        .unwrap();
    match &results[0] {
        SqlResult::Rows(rows) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], Literal::Text("bob".into()));
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn btree_select_before_index_ready_still_returns_correct_full_result() {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();

    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
        .unwrap();
    engine
        .execute_sql(xid, "CREATE INDEX idx ON t USING BTREE (id)")
        .unwrap();
    for i in 0..50 {
        engine
            .execute_sql(
                xid,
                &format!("INSERT INTO t (id, name) VALUES ({i}, 'row{i}')"),
            )
            .unwrap();
    }
    engine.commit(xid).unwrap();

    // The durable B-Tree (P3.a) is built synchronously as part of each INSERT,
    // so it is always crash-consistent with committed data — there is no
    // backfill window to race. An equality query must return the exact match,
    // whether served by the index or (if the column had no index) a full scan.
    let xid2 = engine.begin().unwrap();
    let results = engine
        .execute_sql(xid2, "SELECT name FROM t WHERE id = 17")
        .unwrap();
    match &results[0] {
        SqlResult::Rows(rows) => {
            assert_eq!(rows.len(), 1, "must find the exact match even pre-Ready");
            assert_eq!(rows[0][0], Literal::Text("row17".into()));
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn near_query_before_index_ready_does_not_error() {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();

    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
        .unwrap();
    engine
        .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
        .unwrap();
    for i in 0..50 {
        engine
            .execute_sql(
                xid,
                &format!("INSERT INTO t (id, embedding) VALUES ({i}, [{i}.0, {i}.0])"),
            )
            .unwrap();
    }
    engine.commit(xid).unwrap();

    // Deliberately not waiting for `IndexStatus::Ready` — a `NEAR` query
    // racing the worker must not error, and must return only entries the
    // worker has processed so far (possibly fewer than `k`, never a panic
    // or `Err`).
    let xid2 = engine.begin().unwrap();
    let results = engine
        .execute_sql(
            xid2,
            "SELECT id FROM t WHERE NEAR(embedding, [0.0, 0.0], 3)",
        )
        .unwrap();
    match &results[0] {
        SqlResult::Rows(rows) => assert!(rows.len() <= 3),
        other => panic!("expected Rows, got {other:?}"),
    }
}
