// Item 118 — async-HNSW crash-durability reconciliation.
//
// On the served / `open_arc` path a vector INSERT commits the heap row durably,
// then ENQUEUES the HNSW index insert onto the worker's in-memory channel. The
// worker WAL-logs each insert when it runs, so WAL redo restores everything the
// worker durably applied — but the un-drained channel tail is lost to a crash
// (it was never WAL'd). Before item 118 nothing re-derived it, so committed rows
// could be permanently missing from the index and invisible to NEAR.
//
// These tests prove the fix: after an unclean shutdown, a background
// reconcile-on-open re-enqueues the committed-but-unindexed rows so the index
// converges; and a clean shutdown drains + clears the `hnsw.dirty` marker so the
// next open skips reconciliation (O(1) open preserved).

use std::collections::HashSet;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use unidb::sql::executor::ExecResult as SqlResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

const N: usize = 60;
const DIM: usize = 64;

/// Deterministic pseudo-random f32 vector (LCG seeded with `seed`).
fn pseudo_rand_vec(seed: u32, dim: usize) -> Vec<f32> {
    let mut s = seed.wrapping_add(1);
    (0..dim)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s as f32 / u32::MAX as f32) * 2.0 - 1.0
        })
        .collect()
}

fn vec_sql(v: &[f32]) -> String {
    let inner = v
        .iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

/// Set of ids a NEAR(query, k) returns. With ef_search=200 (default) and a small
/// index, k=N explores the whole graph, so the returned set == the set of ids
/// actually present in the HNSW index. A committed heap row missing from the
/// index cannot appear here (NEAR only traverses the graph) — that is exactly the
/// bug this test guards.
fn near_id_set(engine: &Engine, query: &[f32], k: usize) -> HashSet<i64> {
    let xid = engine.begin().unwrap();
    let sql = format!(
        "SELECT id FROM t WHERE NEAR(embedding, {}, {k})",
        vec_sql(query)
    );
    let res = engine.execute_sql(xid, &sql).unwrap();
    engine.abort(xid).unwrap();
    match &res[0] {
        SqlResult::Rows { rows, .. } => rows
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
fn hnsw_reconciles_committed_rows_after_unclean_shutdown() {
    let dir = tempdir().unwrap();
    let query = pseudo_rand_vec(999_999, DIM);

    // ── Phase 1: async engine; insert N committed vector rows; simulate a crash
    // by dropping WITHOUT wait_hnsw_idle / flush_hnsw_for_shutdown, so the
    // worker's queue tail is lost and the hnsw.dirty marker persists.
    {
        let engine = Engine::open_arc(dir.path(), 0).unwrap();
        let sx = engine.begin().unwrap();
        engine
            .execute_sql(
                sx,
                &format!("CREATE TABLE t (id INT, embedding VECTOR({DIM}))"),
            )
            .unwrap();
        engine
            .execute_sql(sx, "CREATE INDEX iv ON t USING HNSW (embedding)")
            .unwrap();
        engine.commit(sx).unwrap();
        for i in 0..N {
            let v = pseudo_rand_vec(i as u32, DIM);
            let xid = engine.begin().unwrap();
            engine
                .execute_sql(
                    xid,
                    &format!(
                        "INSERT INTO t (id, embedding) VALUES ({i}, {})",
                        vec_sql(&v)
                    ),
                )
                .unwrap();
            engine.commit(xid).unwrap();
        }
        // Crash: the worker holds a Weak<Engine> that can no longer upgrade once
        // this Arc drops, so the un-drained tail is never inserted (never WAL'd).
        // (Observed with diagnostics during development: ~27 rows still queued
        // here, and only ~34/60 NEAR-findable immediately after reopen — the lost
        // tail that reconciliation must repair.)
        drop(engine);
    }

    // An unclean shutdown MUST leave the dirty marker (the reconcile gate).
    assert!(
        dir.path().join("hnsw.dirty").exists(),
        "hnsw.dirty marker must persist after an unclean shutdown"
    );

    // ── Phase 2: reopen (async). spawn_hnsw_worker sees the marker and kicks off
    // a background reconcile that re-enqueues the missing rows; the worker indexes
    // them. Poll until NEAR finds all N committed ids (convergence).
    let engine = Engine::open_arc(dir.path(), 0).unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut found;
    loop {
        // Drain whatever reconcile has enqueued so far, then observe.
        engine.wait_hnsw_idle();
        found = near_id_set(&engine, &query, N).len();
        if found == N {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "HNSW index did not converge after crash: only {found}/{N} committed rows are \
             NEAR-findable — reconcile-on-open failed to repair the lost tail"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        found, N,
        "every committed row must be present in the HNSW index after reconciliation"
    );

    // ── A clean shutdown drains + clears the marker, so the next open is O(1)
    // (skips reconciliation).
    engine.flush_hnsw_for_shutdown();
    assert!(
        !dir.path().join("hnsw.dirty").exists(),
        "flush_hnsw_for_shutdown must clear the dirty marker on a clean shutdown"
    );
    drop(engine);
}

#[test]
fn hnsw_clean_shutdown_clears_marker_so_reopen_skips_reconcile() {
    let dir = tempdir().unwrap();

    // Open async, insert a few rows, drain, then shut down cleanly.
    {
        let engine = Engine::open_arc(dir.path(), 0).unwrap();
        let sx = engine.begin().unwrap();
        engine
            .execute_sql(
                sx,
                &format!("CREATE TABLE t (id INT, embedding VECTOR({DIM}))"),
            )
            .unwrap();
        engine
            .execute_sql(sx, "CREATE INDEX iv ON t USING HNSW (embedding)")
            .unwrap();
        engine.commit(sx).unwrap();
        for i in 0..10 {
            let v = pseudo_rand_vec(i as u32, DIM);
            let xid = engine.begin().unwrap();
            engine
                .execute_sql(
                    xid,
                    &format!(
                        "INSERT INTO t (id, embedding) VALUES ({i}, {})",
                        vec_sql(&v)
                    ),
                )
                .unwrap();
            engine.commit(xid).unwrap();
        }
        // Clean shutdown: drain this engine's worker and clear the marker.
        // (No assertion on hnsw_queue_depth() here — it is a PROCESS-GLOBAL
        // counter shared with any other concurrently-running test, so `== 0`
        // would be flaky under parallel test execution. The DB-local checks
        // below — marker cleared, all rows still indexed — are the real proof.)
        engine.flush_hnsw_for_shutdown();
        drop(engine);
    }

    // After a clean shutdown the marker is gone, so a reopen has nothing to
    // reconcile (the index is already complete and durable).
    assert!(
        !dir.path().join("hnsw.dirty").exists(),
        "clean shutdown must have cleared the dirty marker"
    );

    let engine = Engine::open_arc(dir.path(), 0).unwrap();
    engine.wait_hnsw_idle();
    let query = pseudo_rand_vec(0, DIM);
    let found = near_id_set(&engine, &query, 10).len();
    assert_eq!(
        found, 10,
        "all rows must still be indexed after a clean reopen"
    );
    engine.flush_hnsw_for_shutdown();
}
