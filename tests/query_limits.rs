//! P5.f resource control, end-to-end through `Engine::execute_sql_with_limits`:
//! a query honors a wall-clock timeout, a cancellation token, and a per-query
//! `work_mem` budget (which forces the `ORDER BY` spill path).

use std::thread;
use std::time::Duration;

use unidb::query_limits::{CancelToken, QueryLimits};
use unidb::{DbError, Engine};

/// Seed a table with `n` rows and return the engine.
fn seed(dir: &std::path::Path, n: usize) -> Engine {
    let engine = Engine::open(dir, 0).unwrap();
    engine.set_deferred_sync(true); // batch the seed's fsyncs (group commit)
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE t (id INT, v INT)")
        .unwrap();
    for i in 0..n {
        engine
            .execute_sql(
                xid,
                &format!("INSERT INTO t (id, v) VALUES ({i}, {})", n - i),
            )
            .unwrap();
    }
    engine.commit(xid).unwrap();
    engine
}

#[test]
fn zero_timeout_aborts_a_scan() {
    let dir = tempfile::tempdir().unwrap();
    let engine = seed(dir.path(), 4000);

    let xid = engine.begin().unwrap();
    // A deadline already in the past → the scan trips `check()` on its first
    // batch and returns QueryTimeout instead of the rows.
    let res = engine.execute_sql_with_limits(
        xid,
        "SELECT * FROM t",
        QueryLimits::with_timeout(Duration::from_millis(0)),
    );
    engine.commit(xid).unwrap();
    assert!(
        matches!(res, Err(DbError::QueryTimeout { .. })),
        "expected QueryTimeout, got {res:?}"
    );
}

#[test]
fn generous_timeout_completes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = seed(dir.path(), 500);

    let xid = engine.begin().unwrap();
    let res = engine
        .execute_sql_with_limits(
            xid,
            "SELECT * FROM t",
            QueryLimits::with_timeout(Duration::from_secs(30)),
        )
        .unwrap();
    engine.commit(xid).unwrap();
    match &res[0] {
        unidb::sql::executor::ExecResult::Rows { rows, .. } => assert_eq!(rows.len(), 500),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn cancel_token_aborts_a_running_query() {
    let dir = tempfile::tempdir().unwrap();
    let engine = seed(dir.path(), 4000);

    // Pre-cancelled token → the scan stops at its first check point.
    let token = CancelToken::new();
    token.cancel();

    let xid = engine.begin().unwrap();
    let res = engine.execute_sql_with_limits(
        xid,
        "SELECT * FROM t",
        QueryLimits::default().set_cancel(token),
    );
    engine.commit(xid).unwrap();
    assert!(
        matches!(res, Err(DbError::QueryCancelled)),
        "expected QueryCancelled, got {res:?}"
    );
}

#[test]
fn cancel_from_another_thread_is_observed() {
    let dir = tempfile::tempdir().unwrap();
    let engine = seed(dir.path(), 200);

    // A live token cancelled by a helper thread before the query runs; the query
    // must observe it (cross-thread visibility of the shared flag).
    let token = CancelToken::new();
    let t2 = token.clone();
    let h = thread::spawn(move || t2.cancel());
    h.join().unwrap();

    let xid = engine.begin().unwrap();
    let res = engine.execute_sql_with_limits(
        xid,
        "SELECT * FROM t",
        QueryLimits::default().set_cancel(token),
    );
    engine.commit(xid).unwrap();
    assert!(matches!(res, Err(DbError::QueryCancelled)));
}

#[test]
fn per_query_work_mem_forces_order_by_spill_but_stays_correct() {
    let dir = tempfile::tempdir().unwrap();
    let engine = seed(dir.path(), 300);

    // A tiny work_mem forces the external merge-sort spill; the result must
    // still be correctly ordered (the spill is transparent).
    let xid = engine.begin().unwrap();
    let res = engine
        .execute_sql_with_limits(
            xid,
            "SELECT id FROM t ORDER BY id",
            QueryLimits::default().set_work_mem_rows(16),
        )
        .unwrap();
    engine.commit(xid).unwrap();
    match &res[0] {
        unidb::sql::executor::ExecResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 300);
            // Ascending, contiguous 0..300.
            for (i, row) in rows.iter().enumerate() {
                match &row[0] {
                    unidb::sql::logical::Literal::Int(n) => assert_eq!(*n, i as i64),
                    other => panic!("expected int, got {other:?}"),
                }
            }
        }
        other => panic!("expected rows, got {other:?}"),
    }
}
