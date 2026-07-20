//! Integration test for item 70 — sequential scan prefetch (madvise WILLNEED).
//!
//! Tests that:
//! 1. Full-table SELECT (*) over many rows (spanning many pages) returns all
//!    rows correctly when prefetch hints are active.
//! 2. COUNT(*) over the same table returns the correct count.
//! 3. Filtered SELECT also returns the correct subset.
//! 4. The prefetch hint is a correctness-neutral no-op: results must be
//!    identical to what they would be without the hint.
//!
//! We cannot reliably measure wall-clock speedup in a unit test (the
//! environment has warm caches), but we verify that the hint path does not
//! corrupt data, skip rows, or introduce double-counting.

use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::{Engine, SqlResult};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open_engine() -> (tempfile::TempDir, Engine) {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    (dir, engine)
}

fn exec(engine: &Engine, sql: &str) -> Vec<SqlResult> {
    let xid = engine.begin().unwrap();
    let res = engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
    res
}

fn rows_of(res: Vec<SqlResult>) -> Vec<Vec<Literal>> {
    match res.into_iter().next().unwrap() {
        SqlResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn int_val(lit: &Literal) -> i64 {
    match lit {
        Literal::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Insert 1 000 rows across many pages and do a full scan (SELECT *).
/// Verifies that all rows are returned and none are duplicated or skipped —
/// the prefetch hint must be a transparent correctness no-op.
#[test]
fn full_scan_returns_all_rows() {
    let (_dir, engine) = open_engine();

    exec(
        &engine,
        "CREATE TABLE t (id INT PRIMARY KEY, val INT NOT NULL)",
    );

    // Insert in batches of 100 to keep individual transactions small.
    for batch in 0..10i64 {
        let xid = engine.begin().unwrap();
        for i in 0..100i64 {
            let n = batch * 100 + i;
            engine
                .execute_sql(xid, &format!("INSERT INTO t VALUES ({n}, {})", n * 2))
                .unwrap();
        }
        engine.commit(xid).unwrap();
    }

    // Full scan: SELECT * FROM t
    let rows = rows_of(exec(&engine, "SELECT id, val FROM t"));
    assert_eq!(rows.len(), 1_000, "full scan must return all 1 000 rows");

    // Verify no duplicates: collect all ids and check uniqueness.
    let mut ids: Vec<i64> = rows.iter().map(|r| int_val(&r[0])).collect();
    ids.sort_unstable();
    let unique_count = ids.windows(2).filter(|w| w[0] == w[1]).count();
    assert_eq!(unique_count, 0, "full scan must not return duplicate rows");

    // Verify range: ids 0..1000 all present.
    assert_eq!(ids[0], 0, "first id must be 0");
    assert_eq!(ids[999], 999, "last id must be 999");
}

/// COUNT(*) must match the row count returned by a full scan.
#[test]
fn count_star_matches_full_scan() {
    let (_dir, engine) = open_engine();

    exec(
        &engine,
        "CREATE TABLE t (id INT PRIMARY KEY, v INT NOT NULL)",
    );

    for batch in 0..10i64 {
        let xid = engine.begin().unwrap();
        for i in 0..100i64 {
            let n = batch * 100 + i;
            engine
                .execute_sql(xid, &format!("INSERT INTO t VALUES ({n}, {n})"))
                .unwrap();
        }
        engine.commit(xid).unwrap();
    }

    let count_rows = rows_of(exec(&engine, "SELECT COUNT(*) FROM t"));
    assert_eq!(count_rows.len(), 1);
    let count = int_val(&count_rows[0][0]);
    assert_eq!(count, 1_000, "COUNT(*) must equal 1 000");
}

/// Filtered scan (WHERE id >= 500): must return exactly 500 rows with ids
/// 500..999. Prefetch must not cause any rows to be skipped or doubled.
#[test]
fn filtered_scan_correct_subset() {
    let (_dir, engine) = open_engine();

    exec(
        &engine,
        "CREATE TABLE t (id INT PRIMARY KEY, v INT NOT NULL)",
    );

    for batch in 0..10i64 {
        let xid = engine.begin().unwrap();
        for i in 0..100i64 {
            let n = batch * 100 + i;
            engine
                .execute_sql(xid, &format!("INSERT INTO t VALUES ({n}, {n})"))
                .unwrap();
        }
        engine.commit(xid).unwrap();
    }

    let rows = rows_of(exec(&engine, "SELECT id FROM t WHERE id >= 500"));
    assert_eq!(
        rows.len(),
        500,
        "filtered scan must return 500 rows (ids 500..999)"
    );

    let mut ids: Vec<i64> = rows.iter().map(|r| int_val(&r[0])).collect();
    ids.sort_unstable();
    assert_eq!(ids[0], 500, "minimum id in filtered result must be 500");
    assert_eq!(ids[499], 999, "maximum id in filtered result must be 999");
}

/// Scan after reopen — exercises the cold-cache path where the OS has not
/// yet faulted in the pages. The prefetch hint should not cause any panic
/// or data loss when the DB is reopened and scanned immediately.
#[test]
fn scan_after_reopen_correct() {
    let dir = tempdir().unwrap();
    let path = dir.path().to_owned();

    // Phase 1: write 1 000 rows and close.
    {
        let engine = Engine::open(&path, 0).unwrap();
        exec(
            &engine,
            "CREATE TABLE t (id INT PRIMARY KEY, v INT NOT NULL)",
        );
        for batch in 0..10i64 {
            let xid = engine.begin().unwrap();
            for i in 0..100i64 {
                let n = batch * 100 + i;
                engine
                    .execute_sql(xid, &format!("INSERT INTO t VALUES ({n}, {n})"))
                    .unwrap();
            }
            engine.commit(xid).unwrap();
        }
        // engine drops here — closes the DB
    }

    // Phase 2: reopen and scan — tests the cold-page prefetch path.
    {
        let engine = Engine::open(&path, 0).unwrap();
        let rows = rows_of(exec(&engine, "SELECT id FROM t"));
        assert_eq!(
            rows.len(),
            1_000,
            "scan after reopen must return all 1 000 rows"
        );
    }
}
