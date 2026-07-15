/// Quick WAL B/row validation for items 47 (UPDATE in-place patch) and 44
/// (DELETE batched mini-txn). Runs in seconds, no criterion overhead.
/// Uses deferred-sync for the INSERT setup phase so fsyncs don't dominate.
use std::time::Instant;
use tempfile::tempdir;
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

fn fresh() -> (Engine, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let e = Engine::open(dir.path(), 0).unwrap();
    (e, dir)
}

fn build_table(e: &Engine, rows: u64) {
    // Deferred sync for the INSERT phase: all fsyncs are coalesced into the
    // commit boundary instead of one per row, so setup finishes in seconds.
    e.set_deferred_sync(true);
    let x = e.begin().unwrap();
    e.execute_sql(
        x,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, body TEXT, g INTEGER)",
    )
    .unwrap();
    e.commit(x).unwrap();
    let x = e.begin().unwrap();
    e.execute_sql(x, "CREATE INDEX ON t USING BTREE (k)")
        .unwrap();
    e.commit(x).unwrap();
    // Batch 500 rows per commit so the INSERT setup finishes in seconds.
    let prep = e.prepare("INSERT INTO t VALUES ($1, $2, $3, $4)").unwrap();
    const BATCH: u64 = 500;
    let mut i = 0u64;
    while i < rows {
        let x = e.begin().unwrap();
        for j in i..(i + BATCH).min(rows) {
            e.execute_prepared(
                x,
                &prep,
                &[
                    Literal::Int(j as i64),
                    Literal::Int(j as i64),
                    Literal::Text(format!("body_{j}")),
                    Literal::Int((j % 4) as i64),
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
    // Keep deferred sync on — WAL bytes are identical regardless of fsync mode,
    // and disabling it would cause one fsync per mini-txn inside exec_update
    // (~100ms/call on macOS = extremely slow). rec/s will be higher than
    // production but WAL B/row is the honest signal we measure here.
}

#[test]
fn item47_update_wal_bytes_per_row() {
    // 500 rows (250 updates): fast test for WAL-byte regression.
    // Threshold 570 is tight below the 619 baseline (per-row update_rowid_inplace
    // before item 47), proving patch_many fires and coalesces index FPIs.
    // FPI savings grow with scale: at 500 rows ~25%, at 10k+ rows 50%+.
    let rows: u64 = 500;
    let (e, _dir) = fresh();
    build_table(&e, rows);

    let half = (rows / 2) as i64;
    let wal_before = e.wal_total_bytes_appended();
    let t0 = Instant::now();
    let x = e.begin().unwrap();
    let res = e
        .execute_sql(
            x,
            &format!("UPDATE t SET body = 'updated' WHERE k < {half}"),
        )
        .unwrap();
    e.commit(x).unwrap();
    let elapsed = t0.elapsed();
    let wal_after = e.wal_total_bytes_appended();

    let records = res
        .iter()
        .find_map(|r| {
            if let ExecResult::Updated { count } = r {
                Some(*count)
            } else {
                None
            }
        })
        .unwrap_or(0);
    let wal_bytes = wal_after - wal_before;
    let wal_per_row = if records > 0 {
        wal_bytes / records as u64
    } else {
        0
    };
    let rec_per_sec = records as f64 / elapsed.as_secs_f64();

    println!(
        "\n[item47] UPDATE bulk ({records} rows, k<{half}): {:.1}ms → {rec_per_sec:.0} rec/s  \
         WAL B/row = {wal_per_row}  (baseline before item 47: 619)",
        elapsed.as_millis()
    );
    assert!(records > 0, "UPDATE matched 0 rows");
    assert!(
        wal_per_row < 570,
        "WAL B/row {wal_per_row} >= 570 — item 47 patch_many not firing (baseline 619)"
    );
}

#[test]
fn item44_delete_wal_bytes_per_row() {
    // 10k rows (5000 deletes): delete_many batches heap mini-txns per page.
    // Threshold 150 is well below the 230 baseline (per-row mini-txn).
    let rows: u64 = 10_000;
    let (e, _dir) = fresh();
    build_table(&e, rows);

    let half = (rows / 2) as i64;
    let wal_before = e.wal_total_bytes_appended();
    let t0 = Instant::now();
    let x = e.begin().unwrap();
    let res = e
        .execute_sql(x, &format!("DELETE FROM t WHERE k >= {half}"))
        .unwrap();
    e.commit(x).unwrap();
    let elapsed = t0.elapsed();
    let wal_after = e.wal_total_bytes_appended();

    let records = res
        .iter()
        .find_map(|r| {
            if let ExecResult::Deleted { count } = r {
                Some(*count)
            } else {
                None
            }
        })
        .unwrap_or(0);
    let wal_bytes = wal_after - wal_before;
    let wal_per_row = if records > 0 {
        wal_bytes / records as u64
    } else {
        0
    };
    let rec_per_sec = records as f64 / elapsed.as_secs_f64();

    println!(
        "\n[item44] DELETE selected ({records} rows, k>={half}): {:.1}ms → {rec_per_sec:.0} rec/s  \
         WAL B/row = {wal_per_row}  (baseline before item 44: 230)",
        elapsed.as_millis()
    );
    assert!(records > 0, "DELETE matched 0 rows");
    assert!(
        wal_per_row < 150,
        "WAL B/row {wal_per_row} >= 150 — item 44 delete_many batching may not be firing"
    );
}
