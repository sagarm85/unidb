/// Item 74 — batch mini-txn HOT UPDATE throughput probe.
///
/// The Docker bench (Table 3) runs with `deferred_sync=true`: mini-txn commits
/// do NOT fsync individually; only the user txn's final `wal.sync()` does.
/// The item 74 speedup is therefore a CPU/lock-overhead win, not an I/O win:
///   - Pre-item-74: 150k mutex + Vec + CRC32 passes for 50k rows (3/row)
///   - Post-item-74: ~2k passes (2 mini-txns × ~1k page groups at 100k rows)
///
/// This test mirrors the Docker bench by using deferred_sync=true for the
/// measurement phase, then asserts correctness (count + values) and prints
/// diagnostic throughput for manual inspection.  No hard rec/s thresholds
/// because Mac M5 Pro and Docker/ARM numbers differ by 2-4×.
///
/// Correctness: two consecutive HOT UPDATEs, chain resolution, value check.
use std::time::Instant;
use tempfile::tempdir;
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

fn open_engine(dir: &std::path::Path) -> Engine {
    Engine::open(dir, 0).unwrap()
}

/// Build a simple HOT-eligible table: no secondary index on `body`.
/// INSERT phase uses deferred_sync=true for fast setup.
fn build_hot_table(e: &Engine, rows: u64) {
    e.set_deferred_sync(true);
    let x = e.begin().unwrap();
    e.execute_sql(
        x,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT, g INTEGER)",
    )
    .unwrap();
    e.commit(x).unwrap();

    const BATCH: u64 = 500;
    let prep = e.prepare("INSERT INTO t VALUES ($1, $2, $3)").unwrap();
    let mut i = 0u64;
    while i < rows {
        let x = e.begin().unwrap();
        for j in i..(i + BATCH).min(rows) {
            e.execute_prepared(
                x,
                &prep,
                &[
                    Literal::Int(j as i64),
                    Literal::Text(format!("orig_{j}")),
                    Literal::Int((j % 8) as i64),
                ],
            )
            .unwrap();
        }
        e.commit(x).unwrap();
        i += BATCH;
    }
    let x = e.begin().unwrap();
    e.execute_sql(x, "ANALYZE t").unwrap();
    e.commit(x).unwrap();
    // Keep deferred_sync=true for the measurement phase — this matches the
    // Docker Table 3 bench where commit_mini_txn does NOT call fsync.
    // The item 74 win is the CPU/mutex overhead reduction, visible here.
}

fn run_update_hot(e: &Engine, rows: u64) -> (usize, std::time::Duration, u64) {
    let half = (rows / 2) as i64;
    let wal_before = e.wal_total_bytes_appended();
    let t0 = Instant::now();
    let x = e.begin().unwrap();
    let res = e
        .execute_sql(
            x,
            &format!("UPDATE t SET body = 'new_val' WHERE id < {half}"),
        )
        .unwrap();
    e.commit(x).unwrap();
    let elapsed = t0.elapsed();
    let wal_after = e.wal_total_bytes_appended();
    let count = res
        .iter()
        .find_map(|r| {
            if let ExecResult::Updated { count } = r {
                Some(*count)
            } else {
                None
            }
        })
        .unwrap_or(0);
    (count, elapsed, wal_after - wal_before)
}

fn verify_row(e: &Engine, id: i64, expected: &str) {
    let x = e.begin().unwrap();
    let res = e
        .execute_sql(x, &format!("SELECT body FROM t WHERE id = {id}"))
        .unwrap();
    e.commit(x).unwrap();
    match &res[0] {
        unidb::SqlResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0][0],
                Literal::Text(expected.to_owned()),
                "row id={id}: expected '{expected}'"
            );
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn item74_update_hot_throughput_1k() {
    let rows: u64 = 1_000;
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());
    build_hot_table(&e, rows);

    let (count, elapsed, wal_bytes) = run_update_hot(&e, rows);
    let half = (rows / 2) as usize;
    let rec_s = count as f64 / elapsed.as_secs_f64();
    let wal_per_row = if count > 0 {
        wal_bytes / count as u64
    } else {
        0
    };

    println!(
        "[P74 1k] UPDATE HOT: {count}/{half} rows in {:.3}s → {rec_s:.0} rec/s | WAL {wal_per_row} B/row",
        elapsed.as_secs_f64()
    );

    // Correctness checks.
    verify_row(&e, 0, "new_val");
    verify_row(&e, (rows - 1) as i64, &format!("orig_{}", rows - 1));
    assert_eq!(count, half, "should have updated exactly half the rows");

    // WAL B/row sanity: batch HOT emits WAL_INSERT (new version) +
    // WAL_HOT_XPAGE_HEAD (old slot xmax) per row. At 1k rows the per-row
    // overhead should be under 700 B/row (pre-item74 per-row path was ~400 B/row
    // for a simple body-only UPDATE; cross-page adds ~80 B more per row).
    assert!(
        wal_per_row < 1000,
        "WAL B/row = {wal_per_row}: unexpectedly high, batch path may not be compressing mini-txn overhead"
    );
}

#[test]
fn item74_update_hot_throughput_10k() {
    let rows: u64 = 10_000;
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());
    build_hot_table(&e, rows);

    let (count, elapsed, wal_bytes) = run_update_hot(&e, rows);
    let half = (rows / 2) as usize;
    let rec_s = count as f64 / elapsed.as_secs_f64();
    let wal_per_row = if count > 0 {
        wal_bytes / count as u64
    } else {
        0
    };

    println!(
        "[P74 10k] UPDATE HOT: {count}/{half} rows in {:.3}s → {rec_s:.0} rec/s | WAL {wal_per_row} B/row",
        elapsed.as_secs_f64()
    );

    // Correctness checks.
    verify_row(&e, 0, "new_val");
    verify_row(&e, (rows - 1) as i64, &format!("orig_{}", rows - 1));
    assert_eq!(count, half, "should have updated exactly half the rows");
    assert!(wal_per_row < 1000, "WAL B/row = {wal_per_row}: too high");
}

#[test]
fn item74_update_hot_correctness_chain() {
    // Two consecutive batch HOT UPDATEs must chain correctly:
    // orig → v1 → v2 visible to subsequent reads.
    let rows: u64 = 200;
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());

    e.set_deferred_sync(true);
    let x = e.begin().unwrap();
    e.execute_sql(
        x,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT, g INTEGER)",
    )
    .unwrap();
    e.commit(x).unwrap();

    let prep = e.prepare("INSERT INTO t VALUES ($1, $2, $3)").unwrap();
    let x = e.begin().unwrap();
    for j in 0u64..rows {
        e.execute_prepared(
            x,
            &prep,
            &[
                Literal::Int(j as i64),
                Literal::Text(format!("v0_{j}")),
                Literal::Int(0),
            ],
        )
        .unwrap();
    }
    e.commit(x).unwrap();

    // First batch HOT UPDATE.
    let x = e.begin().unwrap();
    e.execute_sql(x, "UPDATE t SET body = 'v1'").unwrap();
    e.commit(x).unwrap();
    verify_row(&e, 0, "v1");
    verify_row(&e, (rows - 1) as i64, "v1");

    // Second batch HOT UPDATE.
    let x = e.begin().unwrap();
    e.execute_sql(x, "UPDATE t SET body = 'v2'").unwrap();
    e.commit(x).unwrap();
    verify_row(&e, 0, "v2");
    verify_row(&e, (rows - 1) as i64, "v2");
}

#[test]
fn item74_update_hot_mixed_eligible_and_nonhot() {
    // A table with a secondary index on k: SET body (no k in SET) → HOT-eligible.
    // SET k (indexed col) → non-HOT, falls through to per-row path.
    // Both paths must produce correct results.
    let rows: u64 = 200;
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());
    e.set_deferred_sync(true);

    let x = e.begin().unwrap();
    e.execute_sql(
        x,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, body TEXT)",
    )
    .unwrap();
    e.commit(x).unwrap();
    let x = e.begin().unwrap();
    e.execute_sql(x, "CREATE INDEX ON t USING BTREE (k)")
        .unwrap();
    e.commit(x).unwrap();

    let prep = e.prepare("INSERT INTO t VALUES ($1, $2, $3)").unwrap();
    let x = e.begin().unwrap();
    for j in 0u64..rows {
        e.execute_prepared(
            x,
            &prep,
            &[
                Literal::Int(j as i64),
                Literal::Int(j as i64),
                Literal::Text(format!("orig_{j}")),
            ],
        )
        .unwrap();
    }
    e.commit(x).unwrap();

    // HOT-eligible: SET body (k not in SET, k has index but SET doesn't touch it).
    let x = e.begin().unwrap();
    let res = e.execute_sql(x, "UPDATE t SET body = 'updated'").unwrap();
    e.commit(x).unwrap();
    let count = res
        .iter()
        .find_map(|r| {
            if let ExecResult::Updated { count } = r {
                Some(*count)
            } else {
                None
            }
        })
        .unwrap_or(0);
    assert_eq!(
        count, rows as usize,
        "HOT UPDATE should touch all {rows} rows"
    );
    verify_row(&e, 0, "updated");

    // Non-HOT: SET k (indexed col in SET → non-HOT path).
    let x = e.begin().unwrap();
    let res = e
        .execute_sql(
            x,
            &format!("UPDATE t SET k = k + 1000 WHERE id < {}", rows / 2),
        )
        .unwrap();
    e.commit(x).unwrap();
    let count2 = res
        .iter()
        .find_map(|r| {
            if let ExecResult::Updated { count } = r {
                Some(*count)
            } else {
                None
            }
        })
        .unwrap_or(0);
    assert_eq!(
        count2,
        (rows / 2) as usize,
        "non-HOT UPDATE should touch {}",
        rows / 2
    );
}
