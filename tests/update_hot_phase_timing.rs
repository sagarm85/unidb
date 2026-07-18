/// Phase-level timing for UPDATE HOT — identifies which of the 3 phases
/// (matching scan, Phase 1 decode/eval/encode, Phase 2 hot_update_many) dominates.
///
/// Uses SELECT to isolate scan cost, and a tiny helper engine to measure
/// just encode/decode in a tight loop.
use std::time::Instant;
use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::Engine;

const ROWS: u64 = 100_000;

fn open_engine(dir: &std::path::Path) -> Engine {
    Engine::open(dir, 0).unwrap()
}

fn build_table(e: &Engine, rows: u64) {
    e.set_deferred_sync(true);
    let x = e.begin().unwrap();
    e.execute_sql(x, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
        .unwrap();
    e.execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
        .unwrap();
    e.commit(x).unwrap();

    const BATCH: u64 = 500;
    let prep = e
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();
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
}

/// Measure SELECT (scan-only cost) vs UPDATE HOT to isolate Phase 2+3 overhead.
#[test]
fn update_hot_phase_timing_select_vs_update() {
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());
    build_table(&e, ROWS);

    let half = (ROWS / 2) as i64;

    // Warm: full-table SELECT to populate OS page cache / mmap mappings.
    let x = e.begin().unwrap();
    let _ = e.execute_sql(x, "SELECT id FROM t WHERE k < 1").unwrap();
    e.commit(x).unwrap();

    // Measure just the scan cost (SELECT returns all 50k matching rows).
    let t_scan = Instant::now();
    for _ in 0..3 {
        let x = e.begin().unwrap();
        let res = e
            .execute_sql(x, &format!("SELECT id FROM t WHERE k < {half}"))
            .unwrap();
        e.commit(x).unwrap();
        let _ = res;
    }
    let scan_ms = t_scan.elapsed().as_secs_f64() * 1000.0 / 3.0;
    println!(
        "[SELECT 3-run avg] {:.1} ms → {:.0} rec/s",
        scan_ms,
        50000.0 / (scan_ms / 1000.0)
    );

    // Now run UPDATE HOT (uses same scan + Phase 1 + Phase 2 + Phase 3).
    let t_upd = Instant::now();
    let x = e.begin().unwrap();
    let _ = e
        .execute_sql(
            x,
            &format!("UPDATE t SET body = 'updated' WHERE k < {half}"),
        )
        .unwrap();
    e.commit(x).unwrap();
    let upd_ms = t_upd.elapsed().as_secs_f64() * 1000.0;
    println!(
        "[UPDATE HOT] {:.1} ms → {:.0} rec/s",
        upd_ms,
        50000.0 / (upd_ms / 1000.0)
    );

    // Run SELECT again after UPDATE (rows are HOT chains — higher resolution cost).
    let t_scan2 = Instant::now();
    for _ in 0..3 {
        let x = e.begin().unwrap();
        let res = e
            .execute_sql(x, &format!("SELECT id FROM t WHERE k < {half}"))
            .unwrap();
        e.commit(x).unwrap();
        let _ = res;
    }
    let scan2_ms = t_scan2.elapsed().as_secs_f64() * 1000.0 / 3.0;
    println!("[SELECT after UPDATE, 3-run avg] {:.1} ms", scan2_ms);

    println!(
        "[Phase 2+3 overhead estimate] {:.1} ms = update {:.1} ms - pre-scan {:.1} ms",
        upd_ms - scan_ms,
        upd_ms,
        scan_ms
    );
    println!(
        "[Phase 2+3 per row] {:.2} µs",
        (upd_ms - scan_ms) * 1000.0 / 50000.0
    );
}

/// Second UPDATE to measure repeated-update cost (pages now have HOT chains).
#[test]
fn update_hot_repeated_update() {
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());
    build_table(&e, ROWS);

    let half = (ROWS / 2) as i64;

    // First UPDATE (cold — all rows inline, no HOT chains yet).
    let t1 = Instant::now();
    let x = e.begin().unwrap();
    let _ = e
        .execute_sql(x, &format!("UPDATE t SET body = 'v1' WHERE k < {half}"))
        .unwrap();
    e.commit(x).unwrap();
    let first_ms = t1.elapsed().as_secs_f64() * 1000.0;

    // Second UPDATE (rows now have cross-page HOT chains — matching is harder).
    let t2 = Instant::now();
    let x = e.begin().unwrap();
    let _ = e
        .execute_sql(x, &format!("UPDATE t SET body = 'v2' WHERE k < {half}"))
        .unwrap();
    e.commit(x).unwrap();
    let second_ms = t2.elapsed().as_secs_f64() * 1000.0;

    println!(
        "[UPDATE #1 (cold)] {:.1} ms → {:.0} rec/s",
        first_ms,
        50000.0 / (first_ms / 1000.0)
    );
    println!(
        "[UPDATE #2 (HOT chains)] {:.1} ms → {:.0} rec/s",
        second_ms,
        50000.0 / (second_ms / 1000.0)
    );
    println!("[HOT chain overhead] {:.1}×", second_ms / first_ms);
}
