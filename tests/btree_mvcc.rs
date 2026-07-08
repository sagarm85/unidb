// The durable-B-Tree analogue of vector_mvcc.rs's proof (M6, reworked for
// P3.a): the on-disk B+tree has no concept of transactions — `apply_durable_
// btree_writes` inserts a `(value, rowid)` entry the moment a row is inserted,
// as its own committed WAL mini-txn, whether or not the surrounding user
// transaction ever commits, and `Engine::abort` has no hook that retracts it
// (index removal is a vacuum-time concern, not an abort-time one). This test
// proves that fact never leaks into a correctness bug for the index-assisted
// `exec_select` path (`try_exec_select_btree` in `sql/executor.rs`): every
// candidate RowId the tree returns is re-checked against MVCC visibility via
// `heap.get` before being included, exactly like `exec_select_near` does for
// vector search.
//
// Unlike the M6 in-memory index this replaces, the durable tree is synchronous
// and always crash-consistent with committed data, so there is no `Ready`
// status to wait on and no background worker to poll — the entry exists the
// instant the INSERT statement returns. That is why this test is *stronger*
// than its predecessor: the doomed entry is not merely "eventually indexed,"
// it is durably present, and the fresh-transaction query below is guaranteed
// to hit the index-assisted path (the column has a durable `index_root`).

use tempfile::tempdir;
use unidb::sql::executor::ExecResult as SqlResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

fn matching_ids(engine: &mut Engine, xid: unidb::format::Xid) -> Vec<i64> {
    let results = engine
        .execute_sql(xid, "SELECT id FROM t WHERE id = 999")
        .unwrap();
    match &results[0] {
        SqlResult::Rows(rows) => rows
            .iter()
            .map(|r| match &r[0] {
                Literal::Int(n) => *n,
                other => panic!("expected Int id, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn aborted_insert_never_surfaces_in_btree_assisted_results() {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();

    let setup_xid = engine.begin().unwrap();
    engine
        .execute_sql(setup_xid, "CREATE TABLE t (id INT, name TEXT)")
        .unwrap();
    engine
        .execute_sql(setup_xid, "CREATE INDEX idx ON t USING BTREE (id)")
        .unwrap();
    engine.commit(setup_xid).unwrap();

    let doomed_xid = engine.begin().unwrap();
    engine
        .execute_sql(doomed_xid, "INSERT INTO t (id, name) VALUES (999, 'ghost')")
        .unwrap();

    // Durable + synchronous: the inserting transaction sees its own uncommitted
    // write through the index-assisted path immediately, no polling.
    assert!(
        matching_ids(&mut engine, doomed_xid).contains(&999),
        "the inserting txn must see its own uncommitted, durably-indexed row"
    );

    // Abort instead of commit. The durable tree's `(999, rowid)` entry is NOT
    // retracted (abort has no index hook — it's a stale hint, scrubbed only by
    // vacuum), so correctness rides entirely on the MVCC re-check below.
    engine.abort(doomed_xid).unwrap();

    // A fresh transaction sees only committed data. If try_exec_select_btree
    // didn't re-check MVCC visibility on every index-sourced candidate, row
    // 999 would leak through here despite never having committed.
    let fresh_xid = engine.begin().unwrap();
    let ids = matching_ids(&mut engine, fresh_xid);
    assert!(
        !ids.contains(&999),
        "aborted insert leaked into BTree-assisted results: {ids:?}"
    );
}

/// P3.a's headline property at the engine level: the durable B-Tree is
/// **reconstructed from disk on reopen with no rebuild**, and index-assisted
/// queries return the same committed rows they did before the restart.
#[test]
fn durable_btree_survives_reopen_without_rebuild() {
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
        for i in 0..50 {
            engine
                .execute_sql(
                    xid,
                    &format!("INSERT INTO t (id, name) VALUES ({i}, 'row{i}')"),
                )
                .unwrap();
        }
        engine.commit(xid).unwrap();
        engine.checkpoint().unwrap();
    }

    // Reopen: `Engine::open` does NOT rescan the heap to rebuild this index —
    // it reads the tree straight from its meta page. The query still works.
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    let xid = engine.begin().unwrap();
    let results = engine
        .execute_sql(xid, "SELECT id FROM t WHERE id = 42")
        .unwrap();
    match &results[0] {
        SqlResult::Rows(rows) => {
            assert_eq!(rows.len(), 1, "durable index must find the committed row");
            assert_eq!(rows[0][0], Literal::Int(42));
        }
        other => panic!("expected Rows, got {other:?}"),
    }
    engine.commit(xid).unwrap();
}
