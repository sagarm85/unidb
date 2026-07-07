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

#[test]
fn engine_restart_rebuilds_fulltext_index_and_search_still_works() {
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
        engine.flush().unwrap();
    }

    let engine2 = Engine::open(dir.path(), 0).unwrap();
    wait_for_ready(&engine2, "t", "body");
    // No SQL-level full-text query surface exists in M2 (only `NEAR` for
    // vectors) — asserting `Ready` after reopen is the correctness bar for
    // this index kind's rebuild, matching the scope note in MEMORY.md.
}

#[test]
fn engine_restart_rebuilds_btree_index_and_select_still_works() {
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

    // Fresh process-equivalent open: the in-memory worker/index from the
    // first `Engine` is gone. Only the catalog's `index: Some(BTree)` flag
    // and the heap's committed rows survived — rebuild-on-open must
    // reconstruct the index from those alone.
    let mut engine2 = Engine::open(dir.path(), 0).unwrap();
    wait_for_ready(&engine2, "t", "id");

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

    // Deliberately not waiting for `IndexStatus::Ready` — unlike `NEAR`'s
    // inherently approximate top-k, an equality/range query must never
    // return an *incomplete* result just because the index backfill is
    // still in progress. `try_exec_select_btree` only trusts the index once
    // `Ready`; before that it falls back to the ordinary full scan, so the
    // result here must be exactly correct, not "possibly fewer rows."
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
