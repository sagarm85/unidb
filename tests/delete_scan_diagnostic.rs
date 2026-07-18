/// DELETE selected — scan-vs-write cost diagnostic (item-74 expert follow-up).
///
/// The Table 3 bench shows DELETE selected at 0.06× PG with cols/row=2,
/// which contradicts the "bottleneck is delete_many page-write phase" note in
/// the honest-ceilings table.  cols/row=2 is the B-tree index path (deform k +
/// heap-fetch per candidate), not the full-scan path.
///
/// Root cause hypothesis: ANALYZE runs at 100k rows, then INSERT adds 100k more
/// rows (k in [N, 2N)).  Stats show k_max < N, so `k >= N` looks ~0% selective
/// → A3 picks B-tree.  At runtime 50% of rows match → B-tree resolves 100k
/// candidates via per-RowId heap fetches.  Postgres uses bitmap heap scan
/// (batch-sorted); unidb does per-RowId random fetch → 14-17× gap.
///
/// This test measures two scenarios:
///   (A) Stale stats  — ANALYZE before INSERT bench, same as Table 3.
///                      A3 fires → B-tree path → per-RowId heap fetches.
///   (B) Fresh stats  — ANALYZE after INSERT bench (both phases).
///                      A3 does NOT fire → parallel full-scan → delete_many.
///
/// Comparing (A) vs (B) isolates the scan-path cost from delete_many.
/// Comparing rec/s with (B) gives the true potential once stats are fresh.
use std::time::Instant;
use tempfile::tempdir;
use unidb::sql::executor::ExecResult;
use unidb::Engine;

const ROWS: u64 = 50_000; // 50k base + 50k INSERT-bench = 100k total (lighter than 100k+100k)
const INSERT_EXTRA: u64 = 50_000; // extra rows with k in [ROWS, 2*ROWS)

fn open_engine(dir: &std::path::Path) -> Engine {
    Engine::open(dir, 0).unwrap()
}

fn build_base_table(e: &Engine, rows: u64) {
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
                    unidb::sql::logical::Literal::Int(j as i64),
                    unidb::sql::logical::Literal::Int(j as i64),
                    unidb::sql::logical::Literal::Text(format!("body_{j}")),
                ],
            )
            .unwrap();
        }
        e.commit(x).unwrap();
        i += BATCH;
    }
}

fn analyze(e: &Engine) {
    let x = e.begin().unwrap();
    e.execute_sql(x, "ANALYZE t").unwrap();
    e.commit(x).unwrap();
}

fn insert_extra_rows(e: &Engine, base: u64, extra: u64) {
    const BATCH: u64 = 500;
    let prep = e.prepare("INSERT INTO t VALUES ($1, $2, $3)").unwrap();
    let mut i = base;
    while i < base + extra {
        let x = e.begin().unwrap();
        for j in i..(i + BATCH).min(base + extra) {
            e.execute_prepared(
                x,
                &prep,
                &[
                    unidb::sql::logical::Literal::Int(j as i64),
                    unidb::sql::logical::Literal::Int(j as i64),
                    unidb::sql::logical::Literal::Text(format!("body_{j}")),
                ],
            )
            .unwrap();
        }
        e.commit(x).unwrap();
        i += BATCH;
    }
}

fn run_delete(e: &Engine, threshold: i64) -> (usize, std::time::Duration) {
    let t0 = Instant::now();
    let x = e.begin().unwrap();
    let res = e
        .execute_sql(x, &format!("DELETE FROM t WHERE k >= {threshold}"))
        .unwrap();
    e.commit(x).unwrap();
    let elapsed = t0.elapsed();
    let count = res
        .iter()
        .find_map(|r| {
            if let ExecResult::Deleted { count } = r {
                Some(*count)
            } else {
                None
            }
        })
        .unwrap_or(0);
    (count, elapsed)
}

/// Scenario A: ANALYZE before INSERT bench, stale stats during DELETE.
/// Mirrors Table 3 exactly.  A3 fires → B-tree path.
#[test]
fn delete_scenario_a_stale_stats() {
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());

    build_base_table(&e, ROWS);
    analyze(&e); // ANALYZE at ROWS rows — k_max < ROWS
    insert_extra_rows(&e, ROWS, INSERT_EXTRA); // add rows k in [ROWS, 2*ROWS)
                                               // No second ANALYZE — stats are stale; DELETE k >= ROWS looks ~0% selective.

    let threshold = ROWS as i64;
    let (count, elapsed) = run_delete(&e, threshold);
    let rec_s = count as f64 / elapsed.as_secs_f64();

    println!(
        "[DELETE diag A] stale-stats (Table 3 mirror): {} rows deleted in {:.3}s → {:.0} rec/s",
        count,
        elapsed.as_secs_f64(),
        rec_s
    );
    assert_eq!(
        count, INSERT_EXTRA as usize,
        "should delete exactly the extra rows"
    );
}

/// Scenario B: ANALYZE after INSERT bench too — fresh stats during DELETE.
/// A3 sees 50% selectivity → does NOT fire → full-scan path (item 66).
/// This measures the TRUE potential of DELETE when stats are not stale.
#[test]
fn delete_scenario_b_fresh_stats() {
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());

    build_base_table(&e, ROWS);
    analyze(&e); // first ANALYZE
    insert_extra_rows(&e, ROWS, INSERT_EXTRA);
    analyze(&e); // second ANALYZE — fresh stats: 50% of rows have k >= ROWS

    let threshold = ROWS as i64;
    let (count, elapsed) = run_delete(&e, threshold);
    let rec_s = count as f64 / elapsed.as_secs_f64();

    println!(
        "[DELETE diag B] fresh-stats (A3 full-scan path): {} rows deleted in {:.3}s → {:.0} rec/s",
        count,
        elapsed.as_secs_f64(),
        rec_s
    );
    assert_eq!(
        count, INSERT_EXTRA as usize,
        "should delete exactly the extra rows"
    );
}

/// Scenario C: full-table DELETE (no predicate) → fast-path truncate.
/// Baseline to confirm delete_many itself is not the bottleneck.
#[test]
fn delete_scenario_c_full_table_baseline() {
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());

    build_base_table(&e, ROWS);
    insert_extra_rows(&e, ROWS, INSERT_EXTRA);

    let total = ROWS + INSERT_EXTRA;
    // Use k >= 0: all rows have k in [0, 2*ROWS) so this matches everything.
    // i64::MIN cannot be used as a SQL literal (parser sees -(9223372036854775808)
    // which overflows i64::MAX during parsing).
    let (count, elapsed) = run_delete(&e, 0);
    let rec_s = count as f64 / elapsed.as_secs_f64();

    println!(
        "[DELETE diag C] full-table (k >= 0, all rows): {} rows in {:.3}s → {:.0} rec/s",
        count,
        elapsed.as_secs_f64(),
        rec_s
    );
    // With no ANALYZE, the full-scan path fires and delete_many batches by page.
    // This isolates the combined scan + delete_many throughput.
    assert_eq!(count, total as usize);
}
