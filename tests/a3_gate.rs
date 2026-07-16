//! Item-43 — A3 gate size-aware selectivity: permanent regression tests.
//!
//! These tests verify *correctness* across the scan-vs-index crossover boundary,
//! not performance.  The A3 gate (`index_lookup_is_selective`, `executor.rs`)
//! decides at runtime whether a range query is better served by a B-tree index
//! lookup or a sequential scan, using a size-aware cost model that depends on
//! `ANALYZE`-gathered `page_count`.
//!
//! Crossover point (50% selectivity, 8 KiB pages, ~133 rows/page):
//!   index wins ⟺ page_count > 4 + matched_rows × 0.012
//!   ≈ total_rows / 133 > 4 + total_rows × 0.006
//!   ≈ total_rows > ~2 600
//!
//! The three table sizes below span both sides:
//!   200 rows  → ~1 page  → gate → scan
//!   1 000 rows → ~8 pages → gate → scan (borderline)
//!   6 000 rows → ~45 pages → gate → index
//!
//! In all cases the query result must be identical.  A fourth case checks that
//! the 50%-selective DELETE regression (that motivated the original 0.3 constant,
//! CLAUDE.md §0.6.5) still returns correct results at the sizes that should
//! stay on the scan path.

use unidb::sql::logical::Literal;
use unidb::{Engine, SqlResult};

fn open(dir: &tempfile::TempDir) -> Engine {
    Engine::open(dir.path(), 0).unwrap()
}

fn exec(engine: &Engine, sql: &str) {
    let xid = engine.begin().unwrap();
    engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
}

fn query_count(engine: &Engine, sql: &str) -> usize {
    let xid = engine.begin().unwrap();
    let res = engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
    match res.into_iter().next().unwrap() {
        SqlResult::Rows { rows, .. } => rows.len(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn query_ints(engine: &Engine, sql: &str) -> Vec<i64> {
    let xid = engine.begin().unwrap();
    let res = engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
    match res.into_iter().next().unwrap() {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match &r[0] {
                Literal::Int(n) => *n,
                other => panic!("expected Int, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// Core helper: build a table of `total_rows` rows with k = 0..total_rows-1,
/// run ANALYZE, then verify that a 50%-selective range SELECT and a 50%-selective
/// DELETE both return the correct row counts.
fn check_crossover_correctness(total_rows: usize) {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);

    exec(&engine, "CREATE TABLE t (id INT, k INT, body TEXT)");
    exec(&engine, "CREATE INDEX ki ON t USING BTREE (k)");

    // Insert rows in one transaction to keep the test fast.
    let xid = engine.begin().unwrap();
    for i in 0..total_rows {
        engine
            .execute_sql(
                xid,
                &format!("INSERT INTO t (id, k, body) VALUES ({i}, {i}, 'row{i}')"),
            )
            .unwrap();
    }
    engine.commit(xid).unwrap();

    // ANALYZE populates page_count so the size-aware gate can fire.
    exec(&engine, "ANALYZE t");

    let half = total_rows / 2;

    // SELECT filtered: `k >= 0 AND k < half` → exactly `half` rows.
    // At small sizes the gate sends this through the sequential scan; at large
    // sizes through the B-tree (with `k < half` preferred over `k >= 0` by
    // `find_best_indexable_btree_predicate`).  Either way the answer must be
    // correct.
    let got = query_count(
        &engine,
        &format!("SELECT id FROM t WHERE k >= 0 AND k < {half}"),
    );
    assert_eq!(
        got, half,
        "total={total_rows}: SELECT k>=0 AND k<{half} returned {got}, expected {half}"
    );

    // Also verify the returned ids are exactly 0..half (content correctness).
    let mut ids = query_ints(
        &engine,
        &format!("SELECT id FROM t WHERE k >= 0 AND k < {half}"),
    );
    ids.sort_unstable();
    let expected: Vec<i64> = (0..half as i64).collect();
    assert_eq!(
        ids, expected,
        "total={total_rows}: SELECT k>=0 AND k<{half} returned wrong ids"
    );

    // 50%-selective DELETE: `k >= half` → removes `half` rows, leaving `half`.
    exec(&engine, &format!("DELETE FROM t WHERE k >= {half}"));

    let remaining = query_count(&engine, "SELECT id FROM t");
    assert_eq!(
        remaining, half,
        "total={total_rows}: after 50%-selective DELETE expected {half} rows, got {remaining}"
    );
}

/// Three table sizes spanning both sides of the scan-vs-index crossover.
/// At 200 and 1 000 rows the gate favours the sequential scan; at 6 000 rows
/// it favours the index.  Correctness must hold in all cases.
#[test]
fn a3_gate_size_swept_crossover_correctness() {
    for total_rows in [200_usize, 1_000, 6_000] {
        check_crossover_correctness(total_rows);
    }
}

/// Without ANALYZE the gate falls back to the legacy fixed-threshold path.
/// Verify that an un-analyzed table still returns correct results (the gate
/// must not crash or silently wrong-answer on a zero page_count).
#[test]
fn a3_gate_no_analyze_still_correct() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);

    exec(&engine, "CREATE TABLE t (id INT, k INT, body TEXT)");
    exec(&engine, "CREATE INDEX ki ON t USING BTREE (k)");

    let xid = engine.begin().unwrap();
    for i in 0..500_i64 {
        engine
            .execute_sql(
                xid,
                &format!("INSERT INTO t (id, k, body) VALUES ({i}, {i}, 'row{i}')"),
            )
            .unwrap();
    }
    engine.commit(xid).unwrap();

    // No ANALYZE → page_count = 0 → legacy threshold fallback.
    let got = query_count(&engine, "SELECT id FROM t WHERE k >= 0 AND k < 250");
    assert_eq!(
        got, 250,
        "no-ANALYZE fallback: expected 250 rows, got {got}"
    );
}

/// Regression guard: a 50%-selective DELETE must stay on the scan path at
/// LARGE table sizes too (above the parallel-SELECT crossover).  Before the
/// serial-vs-parallel cost split, the gate used `HEAP_FETCH_SEQ_EQUIV = 0.012`
/// (tuned for 18-worker parallel SELECT) for serial DELETE too — causing
/// 50%-selective DELETE to wrongly take the index path at large tables.
/// `HEAP_FETCH_SEQ_EQUIV_SERIAL = 0.05` keeps it on the scan path.
///
/// Path signal: `cols_decoded_total` delta.
///   After item 52 Phase B, DELETE's common path (no FK children, no CDC) never
///   calls `decode_row` on matched rows — only `deform_row` for the predicate
///   column(s). So both paths only materialise the 1 predicate column (k):
///
///   Scan path:  every row is deformed (1 col × total_rows):
///               delta ≈ total × 1
///   Index path: only matched candidates are deformed (1 col × half):
///               delta ≈ half × 1
///
///   Schema has 3 columns (id, k, body), 1 predicate column (k):
///   scan ≈ total (= 10000);  index ≈ half (= 5000)
///   Asserting delta > (total + half) / 2 = 7500 distinguishes them cleanly.
#[test]
fn a3_gate_50pct_delete_large_table_stays_on_scan() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);

    exec(&engine, "CREATE TABLE t (id INT, k INT, body TEXT)");
    exec(&engine, "CREATE INDEX ki ON t USING BTREE (k)");

    let total = 10_000_usize; // ~72 pages — well above the parallel-SELECT crossover
    let xid = engine.begin().unwrap();
    for i in 0..total {
        engine
            .execute_sql(
                xid,
                &format!("INSERT INTO t (id, k, body) VALUES ({i}, {i}, 'row{i}')"),
            )
            .unwrap();
    }
    engine.commit(xid).unwrap();
    exec(&engine, "ANALYZE t");

    let half = total / 2;
    // cols_decoded (item 52 Phase B numbers):
    //   scan  ≈ total × 1 = 10000  (deform pred col for every row; no full decode)
    //   index ≈ half  × 1 = 5000   (deform pred col for matched rows only)
    // threshold: (total + half) / 2 = 7500 — scan exceeds it, index falls below.
    let threshold = (total + half) / 2;
    let cols0 = Engine::cols_decoded_total();
    exec(&engine, &format!("DELETE FROM t WHERE k >= {half}"));
    let cols_delta = Engine::cols_decoded_total().saturating_sub(cols0);

    assert!(
        cols_delta > threshold as u64,
        "50%-selective DELETE on large table appears to have taken the index path \
         (cols_decoded delta={cols_delta}, expected >{threshold} for scan path; \
         index path would give ~{})",
        half
    );

    let remaining = query_count(&engine, "SELECT id FROM t");
    assert_eq!(
        remaining, half,
        "50%-selective DELETE on large table: expected {half} rows remaining, got {remaining}"
    );
}

/// Regression: a 50%-selective range DELETE on a small table must NOT take the
/// B-tree path (which regressed it, per CLAUDE.md §0.6.5).  After ANALYZE with
/// the size-aware gate, `page_count <= index_cost` → scan is chosen → no
/// regression.  Verify correctness of the DELETE and of a subsequent SELECT.
#[test]
fn a3_gate_50pct_delete_small_table_correctness() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open(&dir);

    exec(&engine, "CREATE TABLE t (id INT, k INT, body TEXT)");
    exec(&engine, "CREATE INDEX ki ON t USING BTREE (k)");

    let total = 2_000_usize; // ~15 pages — well below the ~2 600-row crossover
    let xid = engine.begin().unwrap();
    for i in 0..total {
        engine
            .execute_sql(
                xid,
                &format!("INSERT INTO t (id, k, body) VALUES ({i}, {i}, 'row{i}')"),
            )
            .unwrap();
    }
    engine.commit(xid).unwrap();
    exec(&engine, "ANALYZE t");

    let half = total / 2;
    // DELETE 50% (`k >= half`).
    exec(&engine, &format!("DELETE FROM t WHERE k >= {half}"));

    let remaining = query_count(&engine, "SELECT id FROM t");
    assert_eq!(
        remaining, half,
        "50%-selective DELETE on small table: expected {half} rows, got {remaining}"
    );

    // Remaining rows must all have k < half.
    let max_k = {
        let mut ks = query_ints(&engine, "SELECT k FROM t");
        ks.sort_unstable();
        *ks.last().unwrap()
    };
    assert!(
        max_k < half as i64,
        "after DELETE k>={half}, found a row with k={max_k}"
    );
}
