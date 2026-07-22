/// Pinpoint the alloc_heap_page bottleneck in cold UPDATE HOT.
///
/// Proof approach: measure INSERT (same alloc pattern as Phase B)
/// vs UPDATE to see if the page-allocation cost dominates.
use std::time::Instant;
use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::Engine;

const ROWS: u64 = 100_000;
const BATCH: u64 = 500;

fn open_engine(dir: &std::path::Path) -> Engine {
    let e = Engine::open(dir, 0).unwrap();
    e.set_deferred_sync(true);
    e
}

/// INSERT with the same number of rows as UPDATE Phase B needs.
/// This measures alloc_heap_page cost in isolation.
#[test]
fn insert_alloc_cost_half_rows() {
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());

    let x = e.begin().unwrap();
    e.execute_sql(x, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
        .unwrap();
    e.commit(x).unwrap();

    let prep = e
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();

    // First: INSERT 100k rows to build the base (cold start).
    let t1 = Instant::now();
    let mut i = 0u64;
    while i < ROWS {
        let x = e.begin().unwrap();
        for j in i..(i + BATCH).min(ROWS) {
            e.execute_prepared(
                x,
                &prep,
                &[
                    Literal::Int(j as i64),
                    Literal::Int(j as i64),
                    Literal::Int((j as i64) % 100),
                    Literal::Text(format!("b{j}")),
                ],
            )
            .unwrap();
        }
        e.commit(x).unwrap();
        i += BATCH;
    }
    let base_ms = t1.elapsed().as_secs_f64() * 1000.0;
    println!(
        "[INSERT 100k (base)] {:.1} ms → {:.0} rec/s | {:.3} µs/row",
        base_ms,
        ROWS as f64 / (base_ms / 1000.0),
        base_ms * 1000.0 / ROWS as f64
    );

    // Now: INSERT 50k more rows (same alloc count as UPDATE Phase B).
    let t2 = Instant::now();
    let mut i = ROWS;
    while i < ROWS + ROWS / 2 {
        let x = e.begin().unwrap();
        for j in i..(i + BATCH).min(ROWS + ROWS / 2) {
            e.execute_prepared(
                x,
                &prep,
                &[
                    Literal::Int(j as i64),
                    Literal::Int(j as i64),
                    Literal::Int((j as i64) % 100),
                    Literal::Text(format!("b{j}")),
                ],
            )
            .unwrap();
        }
        e.commit(x).unwrap();
        i += BATCH;
    }
    let extra_ms = t2.elapsed().as_secs_f64() * 1000.0;
    println!("[INSERT 50k (extra, same alloc count as UPDATE Phase B)] {:.1} ms → {:.0} rec/s | {:.3} µs/row",
        extra_ms, (ROWS/2) as f64 / (extra_ms / 1000.0), extra_ms * 1000.0 / (ROWS/2) as f64);

    println!(
        "[alloc_heap_page cost per alloc] {:.1} µs estimated (extra_ms / ~390 allocs)",
        extra_ms * 1000.0 / 390.0
    );
}

/// Measure WAL total bytes to confirm alloc_heap_page's WAL footprint.
#[test]
fn update_hot_wal_usage() {
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());

    let x = e.begin().unwrap();
    e.execute_sql(x, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
        .unwrap();
    e.execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
        .unwrap();
    e.commit(x).unwrap();

    let prep = e
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();
    let mut i = 0u64;
    while i < ROWS {
        let x = e.begin().unwrap();
        for j in i..(i + BATCH).min(ROWS) {
            e.execute_prepared(
                x,
                &prep,
                &[
                    Literal::Int(j as i64),
                    Literal::Int(j as i64),
                    Literal::Int((j as i64) % 100),
                    Literal::Text(format!("b{j}")),
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

    let half = (ROWS / 2) as i64;

    let wal_before = e.wal_total_bytes_appended();
    let t1 = Instant::now();
    let x = e.begin().unwrap();
    let t_exec1 = Instant::now();
    let _ = e
        .execute_sql(x, &format!("UPDATE t SET body = 'v1' WHERE k < {half}"))
        .unwrap();
    let exec1_ms = t_exec1.elapsed().as_secs_f64() * 1000.0;
    let t_commit1 = Instant::now();
    e.commit(x).unwrap();
    let commit1_ms = t_commit1.elapsed().as_secs_f64() * 1000.0;
    let ms1 = t1.elapsed().as_secs_f64() * 1000.0;
    let wal1 = e.wal_total_bytes_appended() - wal_before;
    println!("[UPDATE #1 cold] {:.1} ms total | exec={:.1}ms commit={:.1}ms | {:.0} rec/s | WAL {wal1} B ({:.0} B/row)",
        ms1, exec1_ms, commit1_ms, (ROWS/2) as f64 / (ms1/1000.0), wal1 as f64 / (ROWS/2) as f64);

    let wal_before2 = e.wal_total_bytes_appended();
    let t2 = Instant::now();
    let x = e.begin().unwrap();
    let t_exec2 = Instant::now();
    let _ = e
        .execute_sql(x, &format!("UPDATE t SET body = 'v2' WHERE k < {half}"))
        .unwrap();
    let exec2_ms = t_exec2.elapsed().as_secs_f64() * 1000.0;
    let t_commit2 = Instant::now();
    e.commit(x).unwrap();
    let commit2_ms = t_commit2.elapsed().as_secs_f64() * 1000.0;
    let ms2 = t2.elapsed().as_secs_f64() * 1000.0;
    let wal2 = e.wal_total_bytes_appended() - wal_before2;
    println!("[UPDATE #2 warm] {:.1} ms total | exec={:.1}ms commit={:.1}ms | {:.0} rec/s | WAL {wal2} B ({:.0} B/row)",
        ms2, exec2_ms, commit2_ms, (ROWS/2) as f64 / (ms2/1000.0), wal2 as f64 / (ROWS/2) as f64);

    // Also time UPDATE #3 (second warm — HOT chains now 2 hops deep)
    let wal_before3 = e.wal_total_bytes_appended();
    let t3 = Instant::now();
    let x = e.begin().unwrap();
    let t_exec3 = Instant::now();
    let _ = e
        .execute_sql(x, &format!("UPDATE t SET body = 'v3' WHERE k < {half}"))
        .unwrap();
    let exec3_ms = t_exec3.elapsed().as_secs_f64() * 1000.0;
    let t_commit3 = Instant::now();
    e.commit(x).unwrap();
    let commit3_ms = t_commit3.elapsed().as_secs_f64() * 1000.0;
    let ms3 = t3.elapsed().as_secs_f64() * 1000.0;
    let wal3 = e.wal_total_bytes_appended() - wal_before3;
    println!("[UPDATE #3 warm2] {:.1} ms total | exec={:.1}ms commit={:.1}ms | {:.0} rec/s | WAL {wal3} B ({:.0} B/row)",
        ms3, exec3_ms, commit3_ms, (ROWS/2) as f64 / (ms3/1000.0), wal3 as f64 / (ROWS/2) as f64);

    println!(
        "[WAL difference #1 vs #2] {} B extra in #1 = {:.1}× more WAL",
        wal1 as i64 - wal2 as i64,
        wal1 as f64 / wal2 as f64
    );
    println!(
        "[Time speedup #2/#1] {:.2}× | WAL speedup ratio] {:.2}×",
        ms1 / ms2,
        wal1 as f64 / wal2 as f64
    );
}
