//! Item 19 G2-join — FULL OUTER JOIN support tests.
//!
//! `FULL OUTER JOIN` preserves every row from *both* sides:
//!  - When a left row has no matching right row, the right columns are NULL.
//!  - When a right row has no matching left row, the left columns are NULL.
//!  - When rows on both sides match, they appear as a normal joined row.
//!
//! The planner routes FULL OUTER to `MergeJoin`, which tracks unmatched rows
//! on both sides (`emit_unmatched_left` + `emit_unmatched_right`).  For
//! `USING` joins the shared column is emitted as `COALESCE(left.col, right.col)`.

use unidb::sql::logical::Literal;
use unidb::{Engine, SqlResult};

// ─── helpers ─────────────────────────────────────────────────────────────────

fn open() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    (engine, dir)
}

fn lit_to_str(l: &Literal) -> String {
    match l {
        Literal::Int(n) => n.to_string(),
        Literal::Text(s) => s.clone(),
        Literal::Bool(b) => b.to_string(),
        Literal::Null => "NULL".to_string(),
        Literal::Float(f) => format!("{f}"),
        other => format!("{other:?}"),
    }
}

/// Execute setup statements then `query`, returning all result rows.
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
    match results.into_iter().next().unwrap() {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.iter().map(lit_to_str).collect())
            .collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// Same as `run`, but sorts the rows so assertions are order-insensitive.
fn run_sorted(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let mut rows = run(setup, query);
    rows.sort();
    rows
}

// ─── Test 1: basic FULL OUTER JOIN — preserves all rows from both sides ───────

#[test]
fn full_outer_basic() {
    // left: (1, 'a'), (2, 'b')
    // right: (2, 'x'), (3, 'y')
    // Expected (FULL OUTER ON l.id = r.id):
    //   (1, 'a', NULL, NULL)  — left-only
    //   (2, 'b', 2, 'x')     — matched
    //   (NULL, NULL, 3, 'y') — right-only
    let setup = [
        "CREATE TABLE left_t  (id INT, lv TEXT)",
        "CREATE TABLE right_t (id INT, rv TEXT)",
        "INSERT INTO left_t  VALUES (1, 'a'), (2, 'b')",
        "INSERT INTO right_t VALUES (2, 'x'), (3, 'y')",
    ];
    let mut rows = run_sorted(
        &setup,
        "SELECT left_t.id, left_t.lv, right_t.id, right_t.rv \
         FROM left_t FULL OUTER JOIN right_t ON left_t.id = right_t.id",
    );
    rows.sort();
    assert_eq!(rows.len(), 3, "FULL OUTER must produce 3 rows");
    // Check that NULL, a matched row, and a right-only row all appear.
    assert!(
        rows.iter().any(|r| r[0] == "NULL" && r[2] == "3"),
        "right-only row (id=3) must appear with NULL left columns: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r[0] == "1" && r[2] == "NULL"),
        "left-only row (id=1) must appear with NULL right columns: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r[0] == "2" && r[2] == "2"),
        "matched row (id=2) must appear: {rows:?}"
    );
}

// ─── Test 2: unmatched LEFT rows appear with NULL right columns ────────────

#[test]
fn full_outer_unmatched_left() {
    let setup = [
        "CREATE TABLE emp   (id INT, name TEXT)",
        "CREATE TABLE dept  (id INT, dname TEXT)",
        "INSERT INTO emp  VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')",
        "INSERT INTO dept VALUES (1, 'Eng')",
    ];
    let rows = run_sorted(
        &setup,
        "SELECT emp.id, dept.dname \
         FROM emp FULL OUTER JOIN dept ON emp.id = dept.id",
    );
    // 3 emp rows — only id=1 matches; id=2 and id=3 appear with NULL dname.
    assert_eq!(rows.len(), 3);
    for r in &rows {
        if r[0] == "1" {
            assert_eq!(r[1], "Eng");
        } else {
            assert_eq!(
                r[1], "NULL",
                "unmatched emp row must have NULL dname: {r:?}"
            );
        }
    }
}

// ─── Test 3: unmatched RIGHT rows appear with NULL left columns ────────────

#[test]
fn full_outer_unmatched_right() {
    let setup = [
        "CREATE TABLE orders   (oid INT, cid INT)",
        "CREATE TABLE customers (cid INT, cname TEXT)",
        "INSERT INTO orders    VALUES (101, 1)",
        "INSERT INTO customers VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')",
    ];
    let rows = run_sorted(
        &setup,
        "SELECT orders.oid, customers.cname \
         FROM orders FULL OUTER JOIN customers ON orders.cid = customers.cid",
    );
    // 3 customer rows — customers 2 and 3 have no order (NULL oid).
    assert_eq!(rows.len(), 3);
    let null_oid: Vec<_> = rows.iter().filter(|r| r[0] == "NULL").collect();
    assert_eq!(
        null_oid.len(),
        2,
        "two right-only customers must appear with NULL oid: {rows:?}"
    );
}

// ─── Test 4: FULL OUTER JOIN … USING — shared column = COALESCE ──────────

#[test]
fn full_outer_using() {
    // USING (id): the merged `id` column must show the non-NULL side's value.
    let setup = [
        "CREATE TABLE ta (id INT, av TEXT)",
        "CREATE TABLE tb (id INT, bv TEXT)",
        "INSERT INTO ta VALUES (1, 'left-only'), (2, 'both')",
        "INSERT INTO tb VALUES (2, 'both-right'), (3, 'right-only')",
    ];
    let rows = run_sorted(
        &setup,
        "SELECT id, av, bv FROM ta FULL OUTER JOIN tb USING (id)",
    );
    // Expected rows (sorted):
    //   id=1, av='left-only',  bv=NULL
    //   id=2, av='both',       bv='both-right'
    //   id=3, av=NULL,         bv='right-only'
    assert_eq!(rows.len(), 3, "USING join must produce 3 rows: {rows:?}");
    // id must never be NULL — COALESCE picks whichever side is non-NULL.
    for r in &rows {
        assert_ne!(r[0], "NULL", "merged USING column must be non-NULL: {r:?}");
    }
    let r1: Vec<_> = rows.iter().find(|r| r[0] == "1").unwrap().clone();
    assert_eq!(r1[1], "left-only");
    assert_eq!(r1[2], "NULL");

    let r2: Vec<_> = rows.iter().find(|r| r[0] == "2").unwrap().clone();
    assert_eq!(r2[1], "both");
    assert_eq!(r2[2], "both-right");

    let r3: Vec<_> = rows.iter().find(|r| r[0] == "3").unwrap().clone();
    assert_eq!(r3[1], "NULL");
    assert_eq!(r3[2], "right-only");
}

// ─── Test 5: empty LEFT table — only right rows appear (with NULL left) ────

#[test]
fn full_outer_no_rows_left() {
    let setup = [
        "CREATE TABLE empty_l (id INT, lv TEXT)",
        "CREATE TABLE right_r (id INT, rv TEXT)",
        "INSERT INTO right_r VALUES (10, 'x'), (20, 'y')",
    ];
    let rows = run_sorted(
        &setup,
        "SELECT empty_l.id, right_r.rv \
         FROM empty_l FULL OUTER JOIN right_r ON empty_l.id = right_r.id",
    );
    // Both right rows must appear; left column is NULL for both.
    assert_eq!(rows.len(), 2, "should get 2 right-only rows: {rows:?}");
    for r in &rows {
        assert_eq!(r[0], "NULL", "left id must be NULL: {r:?}");
        assert!(r[1] == "x" || r[1] == "y");
    }
}

// ─── Test 6: empty RIGHT table — only left rows appear (with NULL right) ────

#[test]
fn full_outer_no_rows_right() {
    let setup = [
        "CREATE TABLE left_l  (id INT, lv TEXT)",
        "CREATE TABLE empty_r (id INT, rv TEXT)",
        "INSERT INTO left_l VALUES (1, 'a'), (2, 'b')",
    ];
    let rows = run_sorted(
        &setup,
        "SELECT left_l.lv, empty_r.rv \
         FROM left_l FULL OUTER JOIN empty_r ON left_l.id = empty_r.id",
    );
    // Both left rows appear; right column is NULL for both.
    assert_eq!(rows.len(), 2, "should get 2 left-only rows: {rows:?}");
    for r in &rows {
        assert_ne!(r[0], "NULL", "left lv must be non-NULL: {r:?}");
        assert_eq!(r[1], "NULL", "right rv must be NULL: {r:?}");
    }
}

// ─── Test 7: all rows match — output equals INNER JOIN (no extra NULLs) ────

#[test]
fn full_outer_all_match() {
    let setup = [
        "CREATE TABLE p (id INT, pv TEXT)",
        "CREATE TABLE q (id INT, qv TEXT)",
        "INSERT INTO p VALUES (1, 'p1'), (2, 'p2')",
        "INSERT INTO q VALUES (1, 'q1'), (2, 'q2')",
    ];
    let rows = run_sorted(
        &setup,
        "SELECT p.id, p.pv, q.qv FROM p FULL OUTER JOIN q ON p.id = q.id",
    );
    // Every row matches; no NULLs expected in the output.
    assert_eq!(rows.len(), 2, "all-match FULL OUTER must produce 2 rows");
    for r in &rows {
        assert!(
            !r.contains(&"NULL".to_string()),
            "no NULLs expected when every row matches: {r:?}"
        );
    }
}

// ─── Test 8: WHERE clause applied after FULL OUTER JOIN ────────────────────

#[test]
fn full_outer_with_where() {
    // Start with 3 FULL OUTER rows (left-only id=1, matched id=2, right-only id=3).
    // WHERE filters to rows where at least one id column equals 2 (the match).
    let setup = [
        "CREATE TABLE tl (id INT, lv TEXT)",
        "CREATE TABLE tr (id INT, rv TEXT)",
        "INSERT INTO tl VALUES (1, 'lonly'), (2, 'lboth')",
        "INSERT INTO tr VALUES (2, 'rboth'), (3, 'ronly')",
    ];
    let rows = run_sorted(
        &setup,
        "SELECT tl.id, tr.id \
         FROM tl FULL OUTER JOIN tr ON tl.id = tr.id \
         WHERE tl.id = 2",
    );
    // WHERE tl.id = 2 must eliminate the left-only row (id=1, right=NULL) and
    // the right-only row (left=NULL), keeping only the matched pair.
    assert_eq!(rows.len(), 1, "WHERE should narrow to 1 row: {rows:?}");
    assert_eq!(rows[0], vec!["2", "2"]);
}
