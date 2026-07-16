// Correctness tests for Heap::update_many (item 56 Step 2).
//
// Five cases:
//   1. Batch path produces identical results to the per-row path.
//   2. Aborting a user txn mid-batch reverses all xmax stamps (Phase A undo).
//   3. UNIQUE tables stay on the per-row path and still enforce constraints.
//   4. Page-boundary crossing (≥3 pages, 256-byte body) — item 50 regression guard.
//   5. Throughput probe at 50k rows — printed via eprintln, not asserted.

use std::time::Instant;

use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::Engine;

fn open(path: &std::path::Path) -> Engine {
    Engine::open(path, 0).unwrap()
}

fn count_where(e: &Engine, xid: u64, table: &str, pred: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {pred}");
    let r = e.execute_sql(xid, &sql).unwrap();
    match &r[0] {
        unidb::SqlResult::Rows { rows, .. } => match &rows[0][0] {
            Literal::Int(n) => *n,
            other => panic!("expected Int, got {other:?}"),
        },
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn count_all(e: &Engine, xid: u64, table: &str) -> i64 {
    count_where(e, xid, table, "1 = 1")
}

// ── test 1: batch UPDATE produces same values as per-row UPDATE ──────────────

#[test]
fn update_many_batch_produces_same_result_as_per_row() {
    // Build two identical databases and run the same UPDATE on each.
    // One uses the per-row path (UNIQUE PRIMARY KEY forces it), the other
    // uses the batch path (no UNIQUE/FK).  Both must end with the same
    // row count and updated values.
    let n = 200i64;
    let new_k = 42i64;

    // Reference: per-row path (PRIMARY KEY forces per-row path).
    let per_row_count = {
        let dir = tempdir().unwrap();
        let e = open(dir.path());
        let xid = e.begin().unwrap();
        e.execute_sql(xid, "CREATE TABLE t (id INT PRIMARY KEY, k INT)")
            .unwrap();
        e.commit(xid).unwrap();
        for chunk in (0..n).collect::<Vec<_>>().chunks(50) {
            let vals = chunk
                .iter()
                .map(|&i| format!("({i},{i})"))
                .collect::<Vec<_>>()
                .join(",");
            let xid = e.begin().unwrap();
            e.execute_sql(xid, &format!("INSERT INTO t (id,k) VALUES {vals}"))
                .unwrap();
            e.commit(xid).unwrap();
        }
        let xid = e.begin().unwrap();
        e.execute_sql(xid, &format!("UPDATE t SET k = {new_k} WHERE id >= 0"))
            .unwrap();
        e.commit(xid).unwrap();
        let xid = e.begin().unwrap();
        let updated = count_where(&e, xid, "t", &format!("k = {new_k}"));
        let total = count_all(&e, xid, "t");
        e.commit(xid).unwrap();
        assert_eq!(total, n, "per-row: total rows must remain {n}");
        updated
    };

    // Batch path: no UNIQUE/FK → uses update_many.
    let batch_count = {
        let dir = tempdir().unwrap();
        let e = open(dir.path());
        let xid = e.begin().unwrap();
        e.execute_sql(xid, "CREATE TABLE t (id INT, k INT)")
            .unwrap();
        e.commit(xid).unwrap();
        for chunk in (0..n).collect::<Vec<_>>().chunks(50) {
            let vals = chunk
                .iter()
                .map(|&i| format!("({i},{i})"))
                .collect::<Vec<_>>()
                .join(",");
            let xid = e.begin().unwrap();
            e.execute_sql(xid, &format!("INSERT INTO t (id,k) VALUES {vals}"))
                .unwrap();
            e.commit(xid).unwrap();
        }
        let xid = e.begin().unwrap();
        e.execute_sql(xid, &format!("UPDATE t SET k = {new_k} WHERE id >= 0"))
            .unwrap();
        e.commit(xid).unwrap();
        let xid = e.begin().unwrap();
        let updated = count_where(&e, xid, "t", &format!("k = {new_k}"));
        let total = count_all(&e, xid, "t");
        e.commit(xid).unwrap();
        assert_eq!(total, n, "batch: total rows must remain {n}");
        updated
    };

    assert_eq!(
        per_row_count, batch_count,
        "batch UPDATE must produce the same updated-row count as per-row UPDATE"
    );
    assert_eq!(per_row_count, n, "all {n} rows must have k = {new_k}");
}

// ── test 2: aborting a user txn reverses all Phase A xmax stamps ─────────────

#[test]
fn update_many_batch_abort_reverses_all_stamps() {
    let n = 200i64;
    let dir = tempdir().unwrap();
    let e = open(dir.path());

    let xid = e.begin().unwrap();
    e.execute_sql(xid, "CREATE TABLE t (id INT, k INT)")
        .unwrap();
    e.commit(xid).unwrap();

    for chunk in (0..n).collect::<Vec<_>>().chunks(50) {
        let vals = chunk
            .iter()
            .map(|&i| format!("({i},{i})"))
            .collect::<Vec<_>>()
            .join(",");
        let xid = e.begin().unwrap();
        e.execute_sql(xid, &format!("INSERT INTO t (id,k) VALUES {vals}"))
            .unwrap();
        e.commit(xid).unwrap();
    }

    // Begin user txn, run UPDATE (batch path), then ABORT.
    let xid = e.begin().unwrap();
    e.execute_sql(xid, "UPDATE t SET k = 999 WHERE id >= 0")
        .unwrap();
    e.abort(xid).unwrap();

    // All rows must still be visible and k must not be 999.
    let xid = e.begin().unwrap();
    let total = count_all(&e, xid, "t");
    let not_updated = count_where(&e, xid, "t", "k != 999");
    e.commit(xid).unwrap();

    assert_eq!(total, n, "abort must restore all {n} rows");
    assert_eq!(
        not_updated, n,
        "abort must revert all k values; none should be 999"
    );
}

// ── test 3: UNIQUE table stays on per-row path and enforces constraints ───────

#[test]
fn update_many_unique_table_stays_on_per_row_path_and_enforces_constraint() {
    let dir = tempdir().unwrap();
    let e = open(dir.path());

    let xid = e.begin().unwrap();
    // PRIMARY KEY forces per-row path via the `has_unique` gate.
    e.execute_sql(xid, "CREATE TABLE t (id INT PRIMARY KEY, k INT)")
        .unwrap();
    e.commit(xid).unwrap();

    let xid = e.begin().unwrap();
    e.execute_sql(xid, "INSERT INTO t (id,k) VALUES (1,10),(2,20),(3,30)")
        .unwrap();
    e.commit(xid).unwrap();

    // Try to update all ids to 1 — violates PRIMARY KEY UNIQUE constraint.
    let xid = e.begin().unwrap();
    let r = e.execute_sql(xid, "UPDATE t SET id = 1 WHERE id >= 1");
    e.abort(xid).unwrap();
    assert!(
        r.is_err(),
        "UPDATE violating PRIMARY KEY must be rejected; got {r:?}"
    );

    // Non-conflicting UPDATE on UNIQUE table must still succeed (per-row path).
    let xid = e.begin().unwrap();
    e.execute_sql(xid, "UPDATE t SET k = 100 WHERE id >= 1")
        .unwrap();
    e.commit(xid).unwrap();

    let xid = e.begin().unwrap();
    let updated = count_where(&e, xid, "t", "k = 100");
    let total = count_all(&e, xid, "t");
    e.commit(xid).unwrap();

    assert_eq!(total, 3, "all 3 rows must survive per-row UPDATE");
    assert_eq!(
        updated, 3,
        "all 3 k values must be 100 after per-row UPDATE"
    );
}

// ── test 4: page-boundary crossing (item 50 regression guard) ────────────────
//
// Inserts enough 256-byte rows to span ≥3 pages, then UPDATEs all of them.
// If Phase B's inner loop has a progress bug (stalls on the same fill page),
// the test will hang and be caught by the test-runner timeout.

#[test]
fn update_many_page_boundary_crossing() {
    // 8 KiB page / (24-byte tuple header + 256-byte body + 4-byte slot) ≈ 28
    // rows per page.  80 rows → ≥3 pages.  256-byte body makes the new
    // versions large enough to force Phase B to spill across multiple fill pages.
    let n = 80i64;
    let body: String = "x".repeat(256);
    let dir = tempdir().unwrap();
    let e = open(dir.path());

    let xid = e.begin().unwrap();
    e.execute_sql(xid, "CREATE TABLE t (id INT, k INT, body TEXT)")
        .unwrap();
    e.commit(xid).unwrap();

    for chunk in (0..n).collect::<Vec<_>>().chunks(20) {
        let vals = chunk
            .iter()
            .map(|&i| format!("({i},{i},'{body}')"))
            .collect::<Vec<_>>()
            .join(",");
        let xid = e.begin().unwrap();
        e.execute_sql(xid, &format!("INSERT INTO t (id,k,body) VALUES {vals}"))
            .unwrap();
        e.commit(xid).unwrap();
    }

    // UPDATE all rows with a new body — batch path, Phase B crosses page boundaries.
    let new_body = "y".repeat(256);
    let xid = e.begin().unwrap();
    e.execute_sql(
        xid,
        &format!("UPDATE t SET body = '{new_body}' WHERE id >= 0"),
    )
    .unwrap();
    e.commit(xid).unwrap();

    let xid = e.begin().unwrap();
    let total = count_all(&e, xid, "t");
    let updated = count_where(&e, xid, "t", &format!("body = '{new_body}'"));
    e.commit(xid).unwrap();

    assert_eq!(total, n, "all {n} rows must survive a page-crossing UPDATE");
    assert_eq!(
        updated, n,
        "all {n} rows must have the new body after UPDATE"
    );
}

// ── test 5: throughput probe — batch UPDATE on 50k rows ──────────────────────
//
// Regression guard: the allocation fix (StagedUpdate dropped Vec<Literal>
// staging; bench showed 0.02× before fix vs 0.05× baseline).  This probe
// prints rec/s to stderr so the number is visible under `--nocapture`.
// Threshold is deliberately low (5 000 rec/s) so it catches catastrophic
// regressions without being environment-sensitive.

#[test]
fn update_many_batch_throughput_probe() {
    let n = 50_000i64;
    let dir = tempdir().unwrap();
    let e = open(dir.path());

    let xid = e.begin().unwrap();
    e.execute_sql(xid, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
        .unwrap();
    e.commit(xid).unwrap();

    let ins = e
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();
    let mut x = e.begin().unwrap();
    for i in 0..n {
        e.execute_prepared(
            x,
            &ins,
            &[
                Literal::Int(i),
                Literal::Int(i),
                Literal::Int(i % 10),
                Literal::Text(format!("b{i}")),
            ],
        )
        .unwrap();
        if (i + 1) % 5_000 == 0 {
            e.commit(x).unwrap();
            x = e.begin().unwrap();
        }
    }
    e.commit(x).unwrap();

    // Warm: bring pages into the buffer pool.
    let x = e.begin().unwrap();
    e.execute_sql(x, "UPDATE t SET body = 'warm' WHERE k < 500")
        .unwrap();
    e.commit(x).unwrap();

    // Measure: batch UPDATE on all n rows (batch gate: no UNIQUE/FK).
    let x = e.begin().unwrap();
    let t0 = Instant::now();
    let res = e
        .execute_sql(x, &format!("UPDATE t SET body = 'updated' WHERE k < {n}"))
        .unwrap();
    e.commit(x).unwrap();
    let elapsed = t0.elapsed().as_secs_f64();

    let count: u64 = res
        .iter()
        .map(|r| match r {
            unidb::SqlResult::Updated { count } => *count as u64,
            _ => 0u64,
        })
        .sum();

    let rate = count as f64 / elapsed;
    eprintln!(
        "\n=== update_many_batch_throughput_probe: {count} rows, {elapsed:.3}s, {rate:.0} rec/s ===\n"
    );

    assert!(
        count >= n as u64 / 2,
        "at least half of rows must be updated"
    );
    assert!(
        rate > 5_000.0,
        "batch UPDATE must exceed 5 000 rec/s; got {rate:.0}"
    );
}
