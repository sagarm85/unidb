//! Item 19 G7 — Window function tests.
//!
//! Tests for `<window_func> OVER (PARTITION BY … ORDER BY …)` support:
//!   - row_number_no_partition      — ROW_NUMBER() OVER (ORDER BY id)
//!   - row_number_with_partition    — ROW_NUMBER() OVER (PARTITION BY dept ORDER BY id)
//!   - rank_with_ties               — RANK() with tied ORDER BY keys produces gaps
//!   - dense_rank_no_gaps           — DENSE_RANK() with ties produces no gaps
//!   - lag_basic                    — LAG(score, 1) OVER (ORDER BY id)
//!   - lag_out_of_bounds            — LAG with n > available rows → NULL
//!   - lead_basic                   — LEAD(score, 1) OVER (ORDER BY id)
//!   - lead_out_of_bounds           — LEAD at end of partition → NULL
//!   - sum_over_partition           — SUM(salary) OVER (PARTITION BY dept)
//!   - avg_over_whole_table         — AVG(score) OVER () → same value in every row
//!   - count_over_partition         — COUNT(*) OVER (PARTITION BY dept)
//!   - min_max_over_partition       — MIN/MAX per partition
//!   - window_with_where            — WHERE applied before window
//!   - row_number_empty_over        — ROW_NUMBER() OVER () (no PARTITION BY or ORDER BY)

use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn open() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    (engine, dir)
}

/// Run setup statements (committed), then execute `query` and return all rows.
fn run(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let (engine, _dir) = open();
    let xid = engine.begin().unwrap();
    for stmt in setup {
        engine.execute_sql(xid, stmt).unwrap();
    }
    engine.commit(xid).unwrap();
    let xid = engine.begin().unwrap();
    let results = engine.execute_sql(xid, query).unwrap();
    engine.commit(xid).unwrap();
    results
        .into_iter()
        .filter_map(|r| {
            if let ExecResult::Rows { rows, .. } = r {
                Some(
                    rows.into_iter()
                        .map(|row| row.iter().map(lit_to_str).collect()),
                )
            } else {
                None
            }
        })
        .flatten()
        .collect()
}

fn lit_to_str(l: &Literal) -> String {
    match l {
        Literal::Int(n) => n.to_string(),
        Literal::Text(s) => s.clone(),
        Literal::Bool(b) => b.to_string(),
        Literal::Null => "NULL".to_string(),
        Literal::Float(f) => {
            // Round to 6 significant figures for stable comparison.
            format!("{:.6}", f)
        }
        other => format!("{other:?}"),
    }
}

// ─── ROW_NUMBER ──────────────────────────────────────────────────────────────

/// `ROW_NUMBER() OVER (ORDER BY id)` assigns 1..n to rows in id order.
#[test]
fn row_number_no_partition() {
    let setup = [
        "CREATE TABLE t (id INT)",
        "INSERT INTO t VALUES (3), (1), (2)",
    ];
    // ORDER BY id in the OVER clause; result should be in heap order so we sort
    // by id to check the window values.
    let rows = run(
        &setup,
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM t ORDER BY id",
    );
    assert_eq!(rows, vec![vec!["1", "1"], vec!["2", "2"], vec!["3", "3"],]);
}

/// `ROW_NUMBER() OVER (PARTITION BY dept ORDER BY id)` resets per department.
#[test]
fn row_number_with_partition() {
    let setup = [
        "CREATE TABLE emp (id INT, dept TEXT)",
        "INSERT INTO emp VALUES (1, 'A'), (2, 'B'), (3, 'A'), (4, 'B'), (5, 'A')",
    ];
    let rows = run(
        &setup,
        "SELECT id, dept, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY id) AS rn \
         FROM emp ORDER BY dept, id",
    );
    assert_eq!(
        rows,
        vec![
            vec!["1", "A", "1"],
            vec!["3", "A", "2"],
            vec!["5", "A", "3"],
            vec!["2", "B", "1"],
            vec!["4", "B", "2"],
        ]
    );
}

// ─── RANK / DENSE_RANK ───────────────────────────────────────────────────────

/// `RANK()` with tied scores produces gaps (1, 1, 3).
#[test]
fn rank_with_ties() {
    let setup = [
        "CREATE TABLE scores (id INT, score INT)",
        "INSERT INTO scores VALUES (1, 100), (2, 100), (3, 90)",
    ];
    let rows = run(
        &setup,
        "SELECT id, score, RANK() OVER (ORDER BY score DESC) AS rnk \
         FROM scores ORDER BY score DESC, id",
    );
    // Two rows tied at 100 both get rank 1; id=3 at 90 gets rank 3 (gap).
    assert_eq!(
        rows,
        vec![
            vec!["1", "100", "1"],
            vec!["2", "100", "1"],
            vec!["3", "90", "3"],
        ]
    );
}

/// `DENSE_RANK()` with ties produces no gaps (1, 1, 2).
#[test]
fn dense_rank_no_gaps() {
    let setup = [
        "CREATE TABLE scores (id INT, score INT)",
        "INSERT INTO scores VALUES (1, 100), (2, 100), (3, 90)",
    ];
    let rows = run(
        &setup,
        "SELECT id, score, DENSE_RANK() OVER (ORDER BY score DESC) AS drnk \
         FROM scores ORDER BY score DESC, id",
    );
    assert_eq!(
        rows,
        vec![
            vec!["1", "100", "1"],
            vec!["2", "100", "1"],
            vec!["3", "90", "2"],
        ]
    );
}

// ─── LAG ─────────────────────────────────────────────────────────────────────

/// `LAG(score, 1) OVER (ORDER BY id)` returns the previous score.
#[test]
fn lag_basic() {
    let setup = [
        "CREATE TABLE t (id INT, score INT)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
    ];
    let rows = run(
        &setup,
        "SELECT id, score, LAG(score, 1) OVER (ORDER BY id) AS prev_score \
         FROM t ORDER BY id",
    );
    assert_eq!(
        rows,
        vec![
            vec!["1", "10", "NULL"], // no previous row
            vec!["2", "20", "10"],
            vec!["3", "30", "20"],
        ]
    );
}

/// `LAG` offset beyond the start of the partition → NULL.
#[test]
fn lag_out_of_bounds() {
    let setup = [
        "CREATE TABLE t (id INT, score INT)",
        "INSERT INTO t VALUES (1, 10), (2, 20)",
    ];
    let rows = run(
        &setup,
        "SELECT id, LAG(score, 2) OVER (ORDER BY id) AS prev2 FROM t ORDER BY id",
    );
    assert_eq!(rows, vec![vec!["1", "NULL"], vec!["2", "NULL"],]);
}

// ─── LEAD ────────────────────────────────────────────────────────────────────

/// `LEAD(score, 1) OVER (ORDER BY id)` returns the next score.
#[test]
fn lead_basic() {
    let setup = [
        "CREATE TABLE t (id INT, score INT)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
    ];
    let rows = run(
        &setup,
        "SELECT id, score, LEAD(score, 1) OVER (ORDER BY id) AS next_score \
         FROM t ORDER BY id",
    );
    assert_eq!(
        rows,
        vec![
            vec!["1", "10", "20"],
            vec!["2", "20", "30"],
            vec!["3", "30", "NULL"], // no next row
        ]
    );
}

/// `LEAD` offset beyond the end of the partition → NULL.
#[test]
fn lead_out_of_bounds() {
    let setup = [
        "CREATE TABLE t (id INT, score INT)",
        "INSERT INTO t VALUES (1, 10), (2, 20)",
    ];
    let rows = run(
        &setup,
        "SELECT id, LEAD(score, 3) OVER (ORDER BY id) AS next3 FROM t ORDER BY id",
    );
    assert_eq!(rows, vec![vec!["1", "NULL"], vec!["2", "NULL"],]);
}

// ─── SUM / AVG over partition ─────────────────────────────────────────────────

/// `SUM(salary) OVER (PARTITION BY dept)` broadcasts the partition sum to each row.
#[test]
fn sum_over_partition() {
    let setup = [
        "CREATE TABLE emp (id INT, dept TEXT, salary INT)",
        "INSERT INTO emp VALUES (1, 'A', 1000), (2, 'B', 2000), (3, 'A', 1500), (4, 'B', 2500)",
    ];
    let rows = run(
        &setup,
        "SELECT id, dept, salary, SUM(salary) OVER (PARTITION BY dept) AS dept_total \
         FROM emp ORDER BY dept, id",
    );
    assert_eq!(
        rows,
        vec![
            vec!["1", "A", "1000", "2500"], // dept A total = 1000 + 1500
            vec!["3", "A", "1500", "2500"],
            vec!["2", "B", "2000", "4500"], // dept B total = 2000 + 2500
            vec!["4", "B", "2500", "4500"],
        ]
    );
}

/// `AVG(score) OVER ()` — empty OVER means whole table; same value in every row.
#[test]
fn avg_over_whole_table() {
    let setup = [
        "CREATE TABLE t (id INT, score INT)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
    ];
    let rows = run(
        &setup,
        "SELECT id, AVG(score) OVER () AS overall_avg FROM t ORDER BY id",
    );
    // average of (10, 20, 30) = 20.0
    assert_eq!(
        rows,
        vec![
            vec!["1", "20.000000"],
            vec!["2", "20.000000"],
            vec!["3", "20.000000"],
        ]
    );
}

// ─── COUNT over partition ────────────────────────────────────────────────────

/// `COUNT(*) OVER (PARTITION BY dept)` = count of rows per department.
#[test]
fn count_over_partition() {
    let setup = [
        "CREATE TABLE emp (id INT, dept TEXT)",
        "INSERT INTO emp VALUES (1, 'A'), (2, 'B'), (3, 'A'), (4, 'B'), (5, 'A')",
    ];
    let rows = run(
        &setup,
        "SELECT id, dept, COUNT(*) OVER (PARTITION BY dept) AS dept_count \
         FROM emp ORDER BY dept, id",
    );
    assert_eq!(
        rows,
        vec![
            vec!["1", "A", "3"], // 3 rows in dept A
            vec!["3", "A", "3"],
            vec!["5", "A", "3"],
            vec!["2", "B", "2"], // 2 rows in dept B
            vec!["4", "B", "2"],
        ]
    );
}

// ─── MIN / MAX over partition ────────────────────────────────────────────────

/// `MIN(score)` and `MAX(score)` OVER (PARTITION BY dept) per department.
#[test]
fn min_max_over_partition() {
    let setup = [
        "CREATE TABLE emp (id INT, dept TEXT, score INT)",
        "INSERT INTO emp VALUES \
         (1, 'A', 80), (2, 'A', 95), (3, 'B', 70), (4, 'B', 85)",
    ];
    let rows = run(
        &setup,
        "SELECT id, dept, \
                MIN(score) OVER (PARTITION BY dept) AS dept_min, \
                MAX(score) OVER (PARTITION BY dept) AS dept_max \
         FROM emp ORDER BY dept, id",
    );
    assert_eq!(
        rows,
        vec![
            vec!["1", "A", "80", "95"],
            vec!["2", "A", "80", "95"],
            vec!["3", "B", "70", "85"],
            vec!["4", "B", "70", "85"],
        ]
    );
}

// ─── WHERE + window ──────────────────────────────────────────────────────────

/// WHERE is applied before window functions; filtered rows don't participate
/// in the window computation.
#[test]
fn window_with_where() {
    let setup = [
        "CREATE TABLE t (id INT, score INT)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30), (4, 40)",
    ];
    // WHERE id > 1 keeps rows 2, 3, 4; ROW_NUMBER starts at 1 for those.
    let rows = run(
        &setup,
        "SELECT id, score, ROW_NUMBER() OVER (ORDER BY id) AS rn \
         FROM t WHERE id > 1 ORDER BY id",
    );
    assert_eq!(
        rows,
        vec![
            vec!["2", "20", "1"],
            vec!["3", "30", "2"],
            vec!["4", "40", "3"],
        ]
    );
}

// ─── ROW_NUMBER() OVER () — no partition, no order ───────────────────────────

/// `ROW_NUMBER() OVER ()` assigns row numbers in arbitrary heap order.
/// We only check that the set of values is {1, 2, 3}.
#[test]
fn row_number_empty_over() {
    let setup = [
        "CREATE TABLE t (id INT)",
        "INSERT INTO t VALUES (10), (20), (30)",
    ];
    let mut rows = run(&setup, "SELECT ROW_NUMBER() OVER () AS rn FROM t");
    rows.sort();
    // Should have exactly three distinct row numbers.
    let rns: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert!(
        rns.contains(&"1") && rns.contains(&"2") && rns.contains(&"3"),
        "expected {{1,2,3}} got {rns:?}"
    );
}
