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
    e.execute_sql(x, "CREATE INDEX ON t USING BTREE (k)").unwrap();
    e.commit(x).unwrap();
    let prep = e.prepare("INSERT INTO t VALUES ($1, $2, $3, $4)").unwrap();
    for i in 0..rows {
        let x = e.begin().unwrap();
        e.execute_prepared(
            x,
            &prep,
            &[
                Literal::Int(i as i64),
                Literal::Int(i as i64),
                Literal::Text(format!("body_{i}")),
                Literal::Int((i % 4) as i64),
            ],
        )
        .unwrap();
        e.commit(x).unwrap();
    }
    let x = e.begin().unwrap();
    e.execute_sql(x, "ANALYZE t").unwrap();
    e.commit(x).unwrap();
    // Re-enable full sync for the actual measurement so WAL bytes are real.
    e.set_deferred_sync(false);
}

#[test]
fn item47_update_wal_bytes_per_row() {
    let rows: u64 = 20_000;
    let (e, _dir) = fresh();
    build_table(&e, rows);

    let half = (rows / 2) as i64;
    let wal_before = e.wal_total_bytes_appended();
    let t0 = Instant::now();
    let x = e.begin().unwrap();
    let res = e
        .execute_sql(x, &format!("UPDATE t SET body = 'updated' WHERE k < {half}"))
        .unwrap();
    e.commit(x).unwrap();
    let elapsed = t0.elapsed();
    let wal_after = e.wal_total_bytes_appended();

    let records = res.iter().find_map(|r| {
        if let ExecResult::Updated { count } = r { Some(*count) } else { None }
    }).unwrap_or(0);
    let wal_bytes = wal_after - wal_before;
    let wal_per_row = if records > 0 { wal_bytes / records as u64 } else { 0 };
    let rec_per_sec = records as f64 / elapsed.as_secs_f64();

    println!(
        "\n[item47] UPDATE bulk ({records} rows, k<{half}): {:.1}ms → {rec_per_sec:.0} rec/s  \
         WAL B/row = {wal_per_row}  (baseline before item 47: 619)",
        elapsed.as_millis()
    );
    assert!(records > 0, "UPDATE matched 0 rows");
    assert!(
        wal_per_row < 300,
        "WAL B/row {wal_per_row} >= 300 — item 47 patch_many may not be firing"
    );
}

#[test]
fn item44_delete_wal_bytes_per_row() {
    let rows: u64 = 20_000;
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

    let records = res.iter().find_map(|r| {
        if let ExecResult::Deleted { count } = r { Some(*count) } else { None }
    }).unwrap_or(0);
    let wal_bytes = wal_after - wal_before;
    let wal_per_row = if records > 0 { wal_bytes / records as u64 } else { 0 };
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
