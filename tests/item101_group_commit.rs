//! Item 101 — WAL group-commit dwell window.
//!
//! Three tests:
//!   1. `single_session_unaffected` — window=0 (default): single-session
//!      sequential inserts complete without crash or regression.
//!   2. `group_commit_window_reduces_fsyncs` — window=2000µs vs window=0,
//!      8 threads × 50 inserts each: the WITH-window scenario must produce
//!      fewer fsyncs than WITHOUT-window. Auto-checkpoint is disabled to
//!      exclude checkpoint fsyncs from the measurement.
//!   3. `runtime_setter_changes_window` — `Engine::set_group_commit_window_us`
//!      is reflected by `Engine::group_commit_window_us`.
//!
//! ## Why we compare scenarios rather than assert an absolute ratio
//!
//! Each INSERT commit triggers two `sync_up_to` calls under the default engine
//! configuration: (1) for the user TXN_COMMIT record and (2) for the item-97
//! row-count catalog mini-txn written after the commit. Both paths go through
//! `flush_lock` and the dwell window, but the catalog LSN comes after the
//! commit LSN, so consecutive threads' catalog syncs can only partially coalesce.
//! This means the raw `commits / fsyncs` ratio < 1 without coalescing and
//! approaches but may not exceed 2 even with strong coalescing. Comparing
//! with-window vs without-window fsyncs directly eliminates that noise.

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::{AutoCheckpointConfig, Engine, SqlResult};

/// Extract a COUNT(*) integer from a `Vec<SqlResult>`.
fn count_from(results: Vec<SqlResult>) -> i64 {
    match results.into_iter().next().unwrap() {
        SqlResult::Rows { rows, .. } => match rows[0][0] {
            Literal::Int(n) => n,
            ref o => panic!("expected Int from COUNT(*), got {o:?}"),
        },
        o => panic!("expected Rows from COUNT(*), got {o:?}"),
    }
}

/// Disable auto-checkpoint so only commit-path fsyncs are counted.
fn disable_checkpoint(engine: &Engine) {
    engine.set_auto_checkpoint_config(AutoCheckpointConfig {
        enabled: false,
        timeout: Duration::from_secs(3600),
        max_wal_size: u64::MAX,
    });
}

/// Run `threads` concurrent inserters, `per_thread` inserts each.
/// Returns (row_count, fsyncs_used).
fn run_concurrent_inserts(engine: Arc<Engine>, threads: usize, per_thread: usize) -> (usize, u64) {
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE t (id INT, thread INT)")
        .unwrap();
    engine.commit(x).unwrap();

    let fsyncs_before = engine.wal_fsyncs_count();
    let barrier = Arc::new(Barrier::new(threads));
    let mut handles = Vec::new();

    for t in 0..threads {
        let engine = Arc::clone(&engine);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..per_thread {
                let x = engine.begin().unwrap();
                engine
                    .execute_sql(x, &format!("INSERT INTO t (id, thread) VALUES ({i}, {t})"))
                    .unwrap();
                engine.commit(x).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let fsyncs_used = engine.wal_fsyncs_count() - fsyncs_before;

    let x = engine.begin().unwrap();
    let rows = engine.execute_sql(x, "SELECT COUNT(*) FROM t").unwrap();
    engine.commit(x).unwrap();
    let count = count_from(rows) as usize;
    (count, fsyncs_used)
}

// ── Test 1 ────────────────────────────────────────────────────────────────────

/// With the default window (0, disabled), single-session sequential inserts
/// must complete without error and without observable throughput collapse.
#[test]
fn single_session_unaffected() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Verify the window defaults to 0.
    assert_eq!(
        engine.group_commit_window_us(),
        0,
        "default window must be 0 (disabled)"
    );

    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE t (id INT, val TEXT)")
        .unwrap();
    engine.commit(x).unwrap();

    let start = Instant::now();
    let n = 200usize;
    for i in 0..n {
        let x = engine.begin().unwrap();
        engine
            .execute_sql(
                x,
                &format!("INSERT INTO t (id, val) VALUES ({i}, 'row{i}')"),
            )
            .unwrap();
        engine.commit(x).unwrap();
    }
    let elapsed = start.elapsed();

    // Sanity: 200 inserts in serial must finish in < 30 s on any reasonable machine.
    assert!(
        elapsed < Duration::from_secs(30),
        "200 inserts took {elapsed:?} — suspiciously slow"
    );

    // Verify all rows visible.
    let x = engine.begin().unwrap();
    let rows = engine.execute_sql(x, "SELECT COUNT(*) FROM t").unwrap();
    engine.commit(x).unwrap();
    let count = count_from(rows);
    assert_eq!(count as usize, n, "all rows must be visible after commit");
}

// ── Test 2 ────────────────────────────────────────────────────────────────────

/// With a 2000µs dwell window and 8 concurrent writers (50 inserts each), the
/// engine must use FEWER fsyncs than the same workload with window=0.
///
/// We compare two scenarios rather than assert an absolute `commits/fsyncs`
/// ratio, because each INSERT commit also triggers an item-97 catalog row-count
/// sync via `sync_up_to(catalog_lsn)` — a second `flush_lock` acquisition that
/// inflates the raw fsync count beyond the commit count. The comparison between
/// window=0 and window=2000µs is unaffected by this inflation: both scenarios
/// pay it equally, so if the window reduces fsyncs in either path the aggregate
/// fsync count drops.
#[test]
fn group_commit_window_reduces_fsyncs() {
    fn run_with_deadline(secs: u64, f: impl FnOnce() + Send + 'static) {
        use std::sync::atomic::{AtomicBool, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let d2 = Arc::clone(&done);
        let h = thread::spawn(move || {
            f();
            d2.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(secs) {
            if done.load(Ordering::SeqCst) {
                h.join().unwrap();
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("test did not finish within {secs}s — possible hang");
    }

    run_with_deadline(120, || {
        let threads = 8usize;
        let per_thread = 50usize;
        let total_inserts = threads * per_thread;

        // ── Scenario A: no window (baseline) ──────────────────────────────
        let dir_a = tempdir().unwrap();
        let engine_a = Arc::new(Engine::open(dir_a.path(), 0).unwrap());
        disable_checkpoint(&engine_a);
        engine_a.set_group_commit_window_us(0);

        let (count_a, fsyncs_a) =
            run_concurrent_inserts(Arc::clone(&engine_a), threads, per_thread);
        assert_eq!(count_a, total_inserts, "window=0: all rows must be durable");

        // ── Scenario B: 2000µs window ─────────────────────────────────────
        let dir_b = tempdir().unwrap();
        let engine_b = Arc::new(Engine::open(dir_b.path(), 0).unwrap());
        disable_checkpoint(&engine_b);
        engine_b.set_group_commit_window_us(2000);

        let (count_b, fsyncs_b) =
            run_concurrent_inserts(Arc::clone(&engine_b), threads, per_thread);
        assert_eq!(
            count_b, total_inserts,
            "window=2000µs: all rows must be durable"
        );

        // With the dwell window the flush-lock leader sleeps 2ms before each
        // fsync, giving the other 7 threads time to append their records so
        // one fsync covers them all. With 8 concurrent writers this should
        // reduce fsyncs meaningfully.
        assert!(
            fsyncs_b < fsyncs_a,
            "expected window=2000µs fsyncs ({fsyncs_b}) < window=0 fsyncs ({fsyncs_a}); \
             coalescing did not reduce fsyncs"
        );

        let ratio_a = total_inserts as f64 / fsyncs_a.max(1) as f64;
        let ratio_b = total_inserts as f64 / fsyncs_b.max(1) as f64;
        // Log for CI visibility.
        eprintln!("window=0:    {total_inserts} commits / {fsyncs_a} fsyncs = {ratio_a:.2}");
        eprintln!("window=2000µs: {total_inserts} commits / {fsyncs_b} fsyncs = {ratio_b:.2}");
    });
}

// ── Test 3 ────────────────────────────────────────────────────────────────────

/// `Engine::set_group_commit_window_us` updates the window reflected by
/// `Engine::group_commit_window_us`. Zero re-disables it.
#[test]
fn runtime_setter_changes_window() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Default.
    assert_eq!(engine.group_commit_window_us(), 0, "default is 0");

    // Enable.
    engine.set_group_commit_window_us(200);
    assert_eq!(
        engine.group_commit_window_us(),
        200,
        "should be 200 after set"
    );

    // Update.
    engine.set_group_commit_window_us(1000);
    assert_eq!(
        engine.group_commit_window_us(),
        1000,
        "should be 1000 after update"
    );

    // Disable.
    engine.set_group_commit_window_us(0);
    assert_eq!(
        engine.group_commit_window_us(),
        0,
        "should be 0 after disable"
    );
}
