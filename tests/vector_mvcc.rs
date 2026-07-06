// The single most important test in M2 (per the approved plan): the
// background index worker has no concept of transactions — it indexes
// whatever `IndexMsg::Upsert` the executor sends it the moment a row is
// inserted, whether or not that row's transaction ever commits. This test
// proves that fact never leaks into a correctness bug: `NEAR`'s
// over-fetch-then-filter execution re-checks every index-sourced candidate
// against MVCC visibility (`exec_select_near` in `sql/executor.rs`), so an
// aborted insert must never surface in a `NEAR` result even if the worker
// indexed it before the abort happened.
//
// Determinism note: rather than sleeping and hoping the worker has caught
// up (a timing-dependent race the approved plan explicitly calls out to
// avoid), this test polls the *inserting transaction's own* `NEAR` query
// (visible to it via ordinary MVCC self-visibility of its own uncommitted
// write) until the worker demonstrably has indexed the row, before
// aborting. That turns "did the worker index it before the abort" from a
// race into a confirmed precondition.

use tempfile::tempdir;
use unidb::sql::executor::ExecResult as SqlResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

fn near_result_ids(engine: &mut Engine, xid: unidb::format::Xid) -> Vec<i64> {
    let results = engine
        .execute_sql(xid, "SELECT id FROM t WHERE NEAR(embedding, [0.0, 0.0], 5)")
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
fn aborted_insert_never_surfaces_in_near_results() {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();

    let setup_xid = engine.begin().unwrap();
    engine
        .execute_sql(setup_xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
        .unwrap();
    engine
        .execute_sql(setup_xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
        .unwrap();
    engine.commit(setup_xid).unwrap();

    let doomed_xid = engine.begin().unwrap();
    engine
        .execute_sql(
            doomed_xid,
            "INSERT INTO t (id, embedding) VALUES (999, [0.0, 0.0])",
        )
        .unwrap();

    // Poll the inserting transaction's own view (MVCC self-visibility of an
    // uncommitted write) until the background worker has demonstrably
    // indexed row 999 — a confirmed precondition, not a timing guess.
    let start = std::time::Instant::now();
    loop {
        if near_result_ids(&mut engine, doomed_xid).contains(&999) {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("worker never indexed the doomed row within timeout");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    // Now abort instead of commit. The row is undone in the heap (M1's
    // abort self-stamps xmax), but the background worker's `VectorIndex`
    // has no concept of transactions — its stale entry for row 999 is not
    // retracted (a known, documented tech-debt item; see MEMORY.md).
    engine.abort(doomed_xid).unwrap();

    // A fresh transaction sees only committed data. If `exec_select_near`
    // didn't re-check MVCC visibility on every index-sourced candidate,
    // row 999 would leak through here despite never having committed.
    let fresh_xid = engine.begin().unwrap();
    let ids = near_result_ids(&mut engine, fresh_xid);
    assert!(
        !ids.contains(&999),
        "aborted insert leaked into NEAR results: {ids:?}"
    );
}
