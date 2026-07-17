//! Parallel scan (Milestone P) correctness: a parallel scan must return the
//! **same set** of rows / the same count as the serial scan, and must honor the
//! MVCC snapshot across an UPDATE/DELETE. Tables here are built large enough to
//! span many heap pages so the parallel path actually engages.

use std::sync::Arc;
use unidb::query_limits::{CancelToken, QueryLimits};
use unidb::sql::logical::Literal;
use unidb::{DbError, Engine, SqlResult};

const ROWS: i64 = 5_000; // ~25+ heap pages at this row size

fn ints(res: &[SqlResult]) -> Vec<i64> {
    let mut out = Vec::new();
    for r in res {
        if let SqlResult::Rows { rows, .. } = r {
            for row in rows {
                if let Some(Literal::Int(n)) = row.first() {
                    out.push(*n);
                }
            }
        }
    }
    out.sort_unstable();
    out
}

fn count(engine: &Arc<Engine>, sql: &str) -> i64 {
    let x = engine.begin().unwrap();
    let res = engine.execute_sql(x, sql).unwrap();
    engine.commit(x).unwrap();
    match &res[0] {
        SqlResult::Rows { rows, .. } => match rows[0][0] {
            Literal::Int(n) => n,
            ref o => panic!("{o:?}"),
        },
        o => panic!("{o:?}"),
    }
}

fn select(engine: &Arc<Engine>, sql: &str) -> Vec<i64> {
    let x = engine.begin().unwrap();
    let res = engine.execute_sql(x, sql).unwrap();
    engine.commit(x).unwrap();
    ints(&res)
}

fn build(engine: &Arc<Engine>) {
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE t (id INT, k INT, body TEXT)")
        .unwrap();
    // A B-tree on k so a `WHERE k >= …` SELECT routes through the index-candidate
    // path (`try_exec_select_btree`) — the filtered-SELECT parallel path.
    engine
        .execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
        .unwrap();
    engine.commit(x).unwrap();
    let ins = engine
        .prepare("INSERT INTO t (id, k, body) VALUES ($1, $2, $3)")
        .unwrap();
    let mut x = engine.begin().unwrap();
    for i in 0..ROWS {
        engine
            .execute_prepared(
                x,
                &ins,
                &[
                    Literal::Int(i),
                    Literal::Int(i),
                    Literal::Text(format!("body-value-number-{i}")),
                ],
            )
            .unwrap();
        if (i + 1) % 1000 == 0 {
            engine.commit(x).unwrap();
            x = engine.begin().unwrap();
        }
    }
    engine.commit(x).unwrap();
}

/// Parallel scan returns identical results to the serial scan for COUNT(*),
/// full SELECT, and a filtered SELECT (that routes through the query engine's
/// base scan).
#[test]
fn parallel_scan_matches_serial() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    build(&engine);

    // Baseline: serial (default off).
    engine.set_parallel_scan(false);
    let s_count = count(&engine, "SELECT COUNT(*) FROM t");
    let s_all = select(&engine, "SELECT id FROM t");
    let s_grouped_count = count(&engine, "SELECT COUNT(*) FROM t WHERE body <> 'x'");
    // Index-served filtered SELECT (the `try_exec_select_btree` candidate path).
    let s_filtered = select(&engine, "SELECT id FROM t WHERE k >= 2000");

    // Parallel on, threshold 1 so a multi-page table always engages it.
    engine.set_parallel_scan(true);
    engine.set_parallel_scan_config(1, 4);
    let p_count = count(&engine, "SELECT COUNT(*) FROM t");
    let p_all = select(&engine, "SELECT id FROM t");
    let p_grouped_count = count(&engine, "SELECT COUNT(*) FROM t WHERE body <> 'x'");
    let p_filtered = select(&engine, "SELECT id FROM t WHERE k >= 2000");

    assert_eq!(s_count, ROWS, "serial count");
    assert_eq!(p_count, s_count, "parallel COUNT(*) matches serial");
    assert_eq!(
        p_all, s_all,
        "parallel full SELECT matches serial (as a set)"
    );
    assert_eq!(p_all.len() as i64, ROWS);
    assert_eq!(
        p_grouped_count, s_grouped_count,
        "parallel filtered COUNT (base-scan path) matches serial"
    );
    assert_eq!(s_grouped_count, ROWS);
    assert_eq!(
        p_filtered, s_filtered,
        "parallel index-candidate SELECT matches serial (as a set)"
    );
    assert_eq!(s_filtered.len() as i64, ROWS - 2000);
}

/// A parallel scan running **while a writer mutates the same table** must never
/// tear a read or panic — reads take owned page copies under the mmap read-lock,
/// so they always see a consistent page. The scan's own snapshot is fixed, so
/// each count is a valid point-in-time value (bounded by the row count).
#[test]
fn parallel_scan_concurrent_with_writer() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    engine.set_concurrent_sql_writes(true);
    build(&engine);
    engine.set_parallel_scan(true);
    engine.set_parallel_scan_config(1, 4);

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let engine = Arc::clone(&engine);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut n = 0i64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let v = 100 + (n % 50);
                let x = engine.begin().unwrap();
                let _ = engine.execute_sql(x, &format!("UPDATE t SET body = 'w{v}' WHERE k = {v}"));
                let _ = engine.commit(x);
                n += 1;
            }
        })
    };

    // Concurrent parallel scans while the writer churns.
    for _ in 0..200 {
        let c = count(&engine, "SELECT COUNT(*) FROM t");
        assert_eq!(c, ROWS, "row count is stable under an UPDATE-only writer");
        let ids = select(&engine, "SELECT id FROM t WHERE k >= 4000");
        assert_eq!(ids.len() as i64, ROWS - 4000);
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
}

/// G1 governance: with a tiny **global** worker budget, many concurrent parallel
/// scans must all still complete correctly and promptly — extra scans degrade to
/// serial rather than oversubscribing or deadlocking. (If admission leaked
/// permits or blocked, this would hang.)
#[test]
fn parallel_scan_global_cap_bounds_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    build(&engine);
    engine.set_parallel_scan(true);
    engine.set_parallel_scan_config(1, 4); // per-query wants up to 4…
    engine.set_parallel_scan_max_total_workers(2); // …but only 2 globally.

    let mut handles = Vec::new();
    for _ in 0..8 {
        let engine = Arc::clone(&engine);
        handles.push(std::thread::spawn(move || {
            for _ in 0..5 {
                assert_eq!(count(&engine, "SELECT COUNT(*) FROM t"), ROWS);
                let ids = select(&engine, "SELECT id FROM t WHERE k >= 4000");
                assert_eq!(ids.len() as i64, ROWS - 4000);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    // The pool is fully released afterward: a lone scan gets its full degree again
    // (correctness is the observable; a leaked permit would show as wrong results
    // over enough iterations, which the loop above would have caught).
    assert_eq!(count(&engine, "SELECT COUNT(*) FROM t"), ROWS);
}

/// G2 governance: a parallel scan honors cancellation — a pre-cancelled token
/// aborts it at the workers' first check point with `QueryCancelled`, exactly
/// like the serial path.
#[test]
fn parallel_scan_honors_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    build(&engine);
    engine.set_parallel_scan(true);
    engine.set_parallel_scan_config(1, 4);

    let token = CancelToken::new();
    token.cancel(); // already cancelled → the scan must abort immediately
    let limits = QueryLimits::default().set_cancel(token);

    let x = engine.begin().unwrap();
    let r = engine.execute_sql_with_limits(x, "SELECT COUNT(*) FROM t", limits);
    let _ = engine.abort(x);
    assert!(
        matches!(r, Err(DbError::QueryCancelled)),
        "a cancelled parallel COUNT must return QueryCancelled, got {r:?}"
    );
}

/// Item 66 — parallel DELETE scan produces the same set of surviving rows as
/// the serial path.
///
/// Uses a 50%-selective predicate (`k >= ROWS/2`) on an indexed column.  At
/// ROWS=10_000 the table spans ~75 heap pages, which exceeds
/// `PARALLEL_CANDIDATE_MIN` (64).  A3's cost model routes 50%-selective
/// deletes to the full-scan path (index cost > scan cost at that selectivity),
/// so the parallel worker path is taken when `parallel_scan` is enabled.
///
/// The test verifies the survivor set is identical regardless of the execution
/// path — correctness is row identity, not just count.
#[test]
fn parallel_delete_matches_serial() {
    const PROWS: i64 = 10_000; // ~75 heap pages at this size → above PARALLEL_CANDIDATE_MIN

    fn build_large(engine: &Arc<Engine>) {
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t (id INT, k INT, body TEXT)")
            .unwrap();
        engine
            .execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
            .unwrap();
        engine.commit(x).unwrap();
        let ins = engine
            .prepare("INSERT INTO t (id, k, body) VALUES ($1, $2, $3)")
            .unwrap();
        let mut x = engine.begin().unwrap();
        for i in 0..PROWS {
            engine
                .execute_prepared(
                    x,
                    &ins,
                    &[
                        Literal::Int(i),
                        Literal::Int(i),
                        Literal::Text(format!("body-{i}")),
                    ],
                )
                .unwrap();
            if (i + 1) % 1000 == 0 {
                engine.commit(x).unwrap();
                x = engine.begin().unwrap();
            }
        }
        engine.commit(x).unwrap();
    }

    // ── Serial baseline ──────────────────────────────────────────────────────
    let dir_s = tempfile::tempdir().unwrap();
    let serial_eng = Arc::new(Engine::open(dir_s.path(), 0).unwrap());
    build_large(&serial_eng);

    serial_eng.set_parallel_scan(false);
    let x = serial_eng.begin().unwrap();
    serial_eng
        .execute_sql(x, &format!("DELETE FROM t WHERE k >= {}", PROWS / 2))
        .unwrap();
    serial_eng.commit(x).unwrap();
    let serial_survivors = select(&serial_eng, "SELECT id FROM t");

    // ── Parallel execution ────────────────────────────────────────────────────
    let dir_p = tempfile::tempdir().unwrap();
    let par_eng = Arc::new(Engine::open(dir_p.path(), 0).unwrap());
    build_large(&par_eng);

    par_eng.set_parallel_scan(true);
    par_eng.set_parallel_scan_config(1, 4);
    let x = par_eng.begin().unwrap();
    par_eng
        .execute_sql(x, &format!("DELETE FROM t WHERE k >= {}", PROWS / 2))
        .unwrap();
    par_eng.commit(x).unwrap();
    let par_survivors = select(&par_eng, "SELECT id FROM t");

    // Both paths must leave exactly the same rows alive.
    assert_eq!(
        serial_survivors.len() as i64,
        PROWS / 2,
        "serial: wrong survivor count"
    );
    assert_eq!(
        par_survivors, serial_survivors,
        "parallel DELETE must leave the same survivors as serial DELETE"
    );
}

/// A parallel scan honors the statement snapshot exactly across an UPDATE (new
/// version + superseded old counts once) and a DELETE (removed row uncounted).
#[test]
fn parallel_scan_honors_mvcc() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    build(&engine);
    engine.set_parallel_scan(true);
    engine.set_parallel_scan_config(1, 4);

    // Update every row's body (new version + superseded old) — count unchanged.
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "UPDATE t SET body = 'updated' WHERE k < 3000")
        .unwrap();
    engine.commit(x).unwrap();
    assert_eq!(
        count(&engine, "SELECT COUNT(*) FROM t"),
        ROWS,
        "count unchanged after UPDATE (one visible version per row)"
    );

    // Delete a chunk — count drops by exactly that many.
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "DELETE FROM t WHERE k < 1000")
        .unwrap();
    engine.commit(x).unwrap();
    assert_eq!(count(&engine, "SELECT COUNT(*) FROM t"), ROWS - 1000);
    let ids = select(&engine, "SELECT id FROM t");
    assert_eq!(ids.len() as i64, ROWS - 1000);
    assert_eq!(ids.first().copied(), Some(1000), "deleted prefix gone");
}
