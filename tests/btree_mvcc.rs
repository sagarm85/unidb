// The BTree analogue of vector_mvcc.rs's proof (M6): the background index
// worker has no concept of transactions — it applies whatever `IndexMsg::
// Upsert` the executor sends it the moment a row is inserted, whether or
// not that row's transaction ever commits. This test proves that fact
// never leaks into a correctness bug for the new index-assisted `exec_select`
// path (`try_exec_select_btree` in `sql/executor.rs`): every candidate
// RowId the `BTreeIndex` returns is re-checked against MVCC visibility via
// `heap.get` before being included, exactly like `exec_select_near` already
// does for vector search.
//
// Unlike `NEAR`, using the index at all is conditional on `IndexStatus::
// Ready` (an in-progress backfill only having *some* rows would silently
// produce an incomplete, wrong result set for an equality/range query,
// unlike NEAR's inherently-approximate top-k). This test confirms `Ready`
// via `Engine::index_status` before the doomed insert, and relies on the
// documented fact that a live `Upsert` on an already-`Ready` entry never
// regresses it back to `Building` (see `index_worker.rs::worker_loop`) —
// so the final, fresh-transaction query below is guaranteed to actually
// exercise the index-assisted path, not silently fall back to a full scan.

use tempfile::tempdir;
use unidb::index_worker::IndexStatus;
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

    // Confirm the index reached Ready (an empty-table backfill marks Ready
    // immediately, but poll rather than assume — the worker is async) before
    // the doomed insert, so the later verification query is guaranteed to
    // exercise the index-assisted path rather than a full-scan fallback.
    let start = std::time::Instant::now();
    loop {
        if engine.index_status("t", "id") == Some(IndexStatus::Ready) {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("index never reached Ready within timeout");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    let doomed_xid = engine.begin().unwrap();
    engine
        .execute_sql(doomed_xid, "INSERT INTO t (id, name) VALUES (999, 'ghost')")
        .unwrap();

    // Poll the inserting transaction's own view (MVCC self-visibility of an
    // uncommitted write) until the background worker has demonstrably
    // indexed row 999 — a confirmed precondition, not a timing guess.
    let start = std::time::Instant::now();
    loop {
        if matching_ids(&mut engine, doomed_xid).contains(&999) {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("worker never indexed the doomed row within timeout");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    // Abort instead of commit. The BTreeIndex's stale entry for row 999 is
    // not retracted (worker has no transaction concept — same documented
    // tech debt as VectorIndex's).
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
