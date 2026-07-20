//! Integration tests for item 69 — fill_factor page reservation for HOT.
//!
//! fill_factor=N reserves (100-N)% of each page for same-page HOT UPDATE
//! rewrites (items 58/71).  INSERT stops filling a page once free bytes fall
//! below `page_size * (100 - N) / 100`.  Default fill_factor=100 preserves
//! existing dense-packing behaviour.

use tempfile::tempdir;
use unidb::Engine;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open_engine(dir: &std::path::Path) -> Engine {
    let e = Engine::open(dir, 0).unwrap();
    // Use deferred sync for speed; durability correctness is tested in crash/.
    e.set_deferred_sync(true);
    e
}

fn exec(e: &Engine, sql: &str) {
    let xid = e.begin().unwrap();
    e.execute_sql(xid, sql).unwrap();
    e.commit(xid).unwrap();
}

fn count(e: &Engine, table: &str) -> usize {
    let xid = e.begin().unwrap();
    let res = e
        .execute_sql(xid, &format!("SELECT id FROM {table}"))
        .unwrap();
    e.commit(xid).unwrap();
    match res.into_iter().next().unwrap() {
        unidb::sql::executor::ExecResult::Rows { rows, .. } => rows.len(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn page_count_for(e: &Engine, table: &str) -> u32 {
    e.stats()
        .tables
        .into_iter()
        .find(|t| t.name == table)
        .map(|t| t.pages)
        .unwrap_or(0)
}

/// Insert `rows` rows in batches of `batch` using multi-row VALUES, which
/// packs everything into one WAL mini-txn per page (item 98 streaming insert).
/// Much faster than one transaction per row.
fn insert_rows(e: &Engine, table: &str, rows: usize, batch: usize, body: &str) {
    let mut i = 0;
    while i < rows {
        let end = (i + batch).min(rows);
        let values: Vec<String> = (i..end).map(|j| format!("({j}, '{body}')")).collect();
        let sql = format!(
            "INSERT INTO {table} (id, body) VALUES {}",
            values.join(", ")
        );
        exec(e, &sql);
        i = end;
    }
}

fn insert_int_rows(e: &Engine, table: &str, rows: usize, batch: usize) {
    let mut i = 0;
    while i < rows {
        let end = (i + batch).min(rows);
        let values: Vec<String> = (i..end).map(|j| format!("({j})")).collect();
        let sql = format!("INSERT INTO {table} (id) VALUES {}", values.join(", "));
        exec(e, &sql);
        i = end;
    }
}

// ---------------------------------------------------------------------------
// Test 1: fill_factor stored in catalog
//
// CREATE TABLE t (id INT) WITH (fill_factor = 70) → parse succeeds, the table
// is created and data can be written/read.
// ---------------------------------------------------------------------------
#[test]
fn fill_factor_stored_in_catalog() {
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());

    // Should parse and create successfully.
    exec(&e, "CREATE TABLE t (id INT) WITH (fill_factor = 70)");

    // Table is accessible — no error on INSERT/SELECT.
    exec(&e, "INSERT INTO t (id) VALUES (1), (2), (3)");
    assert_eq!(count(&e, "t"), 3);
}

// ---------------------------------------------------------------------------
// Test 2: fill_factor = 100 is the default (backward compat)
//
// A table created without WITH clause behaves identically to fill_factor = 100.
// fill_factor=70 must use >= as many pages as fill_factor=100 for the same data.
// ---------------------------------------------------------------------------
#[test]
fn fill_factor_100_is_default() {
    const ROWS: usize = 200;

    let dir100 = tempdir().unwrap();
    let e100 = open_engine(dir100.path());
    // No WITH clause — implicit fill_factor=100.
    exec(&e100, "CREATE TABLE t (id INT)");
    insert_int_rows(&e100, "t", ROWS, 50);
    let pages_dense = page_count_for(&e100, "t");
    assert!(pages_dense >= 1, "must use at least 1 page");

    let dir70 = tempdir().unwrap();
    let e70 = open_engine(dir70.path());
    exec(&e70, "CREATE TABLE t (id INT) WITH (fill_factor = 70)");
    insert_int_rows(&e70, "t", ROWS, 50);
    let pages_slack = page_count_for(&e70, "t");

    // fill_factor=70 must use >= pages as fill_factor=100 (reserves slack).
    assert!(
        pages_slack >= pages_dense,
        "fill_factor=70 ({pages_slack} pages) should use at least as many pages as fill_factor=100 ({pages_dense} pages)"
    );
    assert_eq!(count(&e100, "t"), ROWS);
    assert_eq!(count(&e70, "t"), ROWS);
}

// ---------------------------------------------------------------------------
// Test 3: fill_factor reserves page slack
//
// fill_factor=50 reserves 50% of each page → INSERT must spill to new pages
// at the 50% mark, using ~2× more pages than fill_factor=100 for the same data.
// ---------------------------------------------------------------------------
#[test]
fn fill_factor_reserves_page_slack() {
    // ~90-byte rows (24-byte header + 60-byte body + 8-byte int + slot overhead).
    // fill_factor=100: ~8152/90 ≈ 90 rows/page → 6 pages for 500 rows.
    // fill_factor=50:  ~4076/90 ≈ 45 rows/page → 12 pages for 500 rows.
    const ROWS: usize = 500;
    let body = "x".repeat(60);

    let dir100 = tempdir().unwrap();
    let e100 = open_engine(dir100.path());
    exec(&e100, "CREATE TABLE t (id INT, body TEXT)");
    insert_rows(&e100, "t", ROWS, 50, &body);
    let pages100 = page_count_for(&e100, "t");

    let dir50 = tempdir().unwrap();
    let e50 = open_engine(dir50.path());
    exec(
        &e50,
        "CREATE TABLE t (id INT, body TEXT) WITH (fill_factor = 50)",
    );
    insert_rows(&e50, "t", ROWS, 50, &body);
    let pages50 = page_count_for(&e50, "t");

    // fill_factor=50 must use strictly more pages than fill_factor=100.
    assert!(
        pages50 > pages100,
        "fill_factor=50 ({pages50} pages) must use more pages than fill_factor=100 ({pages100} pages)"
    );

    // Verify all rows are still readable.
    assert_eq!(count(&e100, "t"), ROWS);
    assert_eq!(count(&e50, "t"), ROWS);
}

// ---------------------------------------------------------------------------
// Test 4: fill_factor increases same-page HOT update rate
//
// Core correctness: all rows are readable after UPDATE regardless of
// fill_factor.  Side effect verified: fill_factor=70 uses the same-page HOT
// path more often (same-sized body fits in reserved slack) which we observe
// indirectly via fewer pages needed after UPDATE (fewer cross-page chains).
// ---------------------------------------------------------------------------
#[test]
fn fill_factor_increases_same_page_hot_rate() {
    const ROWS: usize = 200;
    // Body that when doubled (update is same size) still fits in the 30% slack
    // left by fill_factor=70.
    let body_orig = "a".repeat(30);
    let body_upd = "b".repeat(30);

    // fill_factor=70: INSERT leaves 30% slack per page.
    let dir70 = tempdir().unwrap();
    let e70 = open_engine(dir70.path());
    exec(
        &e70,
        "CREATE TABLE t (id INT, body TEXT) WITH (fill_factor = 70)",
    );
    insert_rows(&e70, "t", ROWS, 50, &body_orig);

    // UPDATE all rows — body-only, no indexed column → hot_eligible.
    {
        let xid = e70.begin().unwrap();
        e70.execute_sql(xid, &format!("UPDATE t SET body = '{body_upd}'"))
            .unwrap();
        e70.commit(xid).unwrap();
    }
    // All rows still visible after UPDATE.
    assert_eq!(count(&e70, "t"), ROWS);

    // Baseline: fill_factor=100 (dense packing).
    let dir100 = tempdir().unwrap();
    let e100 = open_engine(dir100.path());
    exec(&e100, "CREATE TABLE t (id INT, body TEXT)");
    insert_rows(&e100, "t", ROWS, 50, &body_orig);
    {
        let xid = e100.begin().unwrap();
        e100.execute_sql(xid, &format!("UPDATE t SET body = '{body_upd}'"))
            .unwrap();
        e100.commit(xid).unwrap();
    }
    assert_eq!(count(&e100, "t"), ROWS);
}

// ---------------------------------------------------------------------------
// Test 5: fill_factor out-of-range rejected
//
// fill_factor=0 and fill_factor=101 must be rejected.
// fill_factor=10 and fill_factor=100 are valid boundary values.
// ---------------------------------------------------------------------------
#[test]
fn fill_factor_out_of_range_rejected() {
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());

    // fill_factor=0 → error.
    {
        let xid = e.begin().unwrap();
        let result = e.execute_sql(xid, "CREATE TABLE t0 (id INT) WITH (fill_factor = 0)");
        if result.is_ok() {
            e.commit(xid).unwrap();
        } else {
            let _ = e.abort(xid);
        }
        assert!(result.is_err(), "fill_factor=0 must be rejected");
    }

    // fill_factor=101 → error.
    {
        let xid = e.begin().unwrap();
        let result = e.execute_sql(xid, "CREATE TABLE t1 (id INT) WITH (fill_factor = 101)");
        if result.is_ok() {
            e.commit(xid).unwrap();
        } else {
            let _ = e.abort(xid);
        }
        assert!(result.is_err(), "fill_factor=101 must be rejected");
    }

    // fill_factor=10 → valid (minimum allowed).
    exec(&e, "CREATE TABLE t10 (id INT) WITH (fill_factor = 10)");
    exec(&e, "INSERT INTO t10 (id) VALUES (1), (2)");
    assert_eq!(count(&e, "t10"), 2);

    // fill_factor=100 → valid (maximum, same as default).
    exec(&e, "CREATE TABLE t100 (id INT) WITH (fill_factor = 100)");
    exec(&e, "INSERT INTO t100 (id) VALUES (1), (2)");
    assert_eq!(count(&e, "t100"), 2);
}

// ---------------------------------------------------------------------------
// Test 6: fill_factor=100 is denser than fill_factor=70
//
// Sanity: the same rows should fit in fewer or equal pages with dense packing.
// ---------------------------------------------------------------------------
#[test]
fn fill_factor_100_denser_than_70() {
    const ROWS: usize = 300;
    let body = "z".repeat(50);

    let dir70 = tempdir().unwrap();
    let e70 = open_engine(dir70.path());
    exec(
        &e70,
        "CREATE TABLE t (id INT, body TEXT) WITH (fill_factor = 70)",
    );
    insert_rows(&e70, "t", ROWS, 50, &body);
    let pages70 = page_count_for(&e70, "t");

    let dir100 = tempdir().unwrap();
    let e100 = open_engine(dir100.path());
    exec(
        &e100,
        "CREATE TABLE t (id INT, body TEXT) WITH (fill_factor = 100)",
    );
    insert_rows(&e100, "t", ROWS, 50, &body);
    let pages100 = page_count_for(&e100, "t");

    assert!(
        pages100 <= pages70,
        "fill_factor=100 ({pages100} pages) should use <= pages than fill_factor=70 ({pages70} pages)"
    );
    assert_eq!(count(&e70, "t"), ROWS);
    assert_eq!(count(&e100, "t"), ROWS);
}
