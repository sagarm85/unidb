/// Item 19 G-NATURAL — `NATURAL JOIN` desugars to `USING` over the shared column set.
///
/// NATURAL JOIN is syntax sugar: it computes the intersection of both sides'
/// column names and rewrites to `USING (shared_cols)`.  No storage/format
/// change — the feature lives entirely in the parser + planner.
use unidb::{Engine, SqlResult};

fn open() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    (engine, dir)
}

fn lit_to_str(lit: &unidb::sql::logical::Literal) -> String {
    use unidb::sql::logical::Literal;
    match lit {
        Literal::Int(n) => n.to_string(),
        Literal::Float(f) => f.to_string(),
        Literal::Text(s) => s.clone(),
        Literal::Bool(b) => b.to_string(),
        Literal::Null => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

/// Execute setup statements (committed) then run `query` in a fresh txn.
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

fn run_sorted(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let mut rows = run(setup, query);
    rows.sort();
    rows
}

fn run_cols(setup: &[&str], query: &str) -> (Vec<String>, Vec<Vec<String>>) {
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
        SqlResult::Rows { columns, rows, .. } => {
            let cols = columns.clone();
            let data = rows
                .into_iter()
                .map(|r| r.iter().map(lit_to_str).collect())
                .collect();
            (cols, data)
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

// ── schema ───────────────────────────────────────────────────────────────────
//
//   employees(id, name, dept_id)   — 4 rows; Dan has no matching dept
//   departments(dept_id, name)     — 3 rows; dept_id=3 has no employees
//
// Shared column: dept_id.

const SETUP_EMP_DEPT: &[&str] = &[
    "CREATE TABLE departments (dept_id INT, dname TEXT)",
    "INSERT INTO departments VALUES (1, 'Engineering'), (2, 'HR'), (3, 'Marketing')",
    "CREATE TABLE employees (id INT, ename TEXT, dept_id INT)",
    "INSERT INTO employees VALUES (1, 'Alice', 1), (2, 'Bob', 1), (3, 'Carol', 2), (4, 'Dan', 99)",
];

// ── tests ────────────────────────────────────────────────────────────────────

/// Basic NATURAL JOIN: shared column `dept_id` matched automatically.
/// Dan (dept_id=99) and Marketing (dept_id=3) have no counterpart → excluded.
#[test]
fn natural_join_basic() {
    let rows = run_sorted(
        SETUP_EMP_DEPT,
        "SELECT ename FROM employees NATURAL JOIN departments ORDER BY ename",
    );
    assert_eq!(rows.len(), 3, "Alice, Bob, Carol match; Dan does not");
    let names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert!(names.contains(&"Alice"));
    assert!(names.contains(&"Bob"));
    assert!(names.contains(&"Carol"));
}

/// The shared column (`dept_id`) must appear exactly once in the output,
/// not duplicated from both sides (same as USING semantics).
#[test]
fn natural_join_shared_col_appears_once() {
    let (cols, rows) = run_cols(
        SETUP_EMP_DEPT,
        "SELECT * FROM employees NATURAL JOIN departments",
    );
    let dept_id_count = cols.iter().filter(|c| *c == "dept_id").count();
    assert_eq!(dept_id_count, 1, "dept_id must appear once, not twice");
    assert_eq!(rows.len(), 3);
}

/// NATURAL JOIN on a fresh pair of tables sharing only `id`.
#[test]
fn natural_join_on_id() {
    let rows = run_sorted(
        &[
            "CREATE TABLE t1 (id INT, val TEXT)",
            "INSERT INTO t1 VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            "CREATE TABLE t2 (id INT, score INT)",
            "INSERT INTO t2 VALUES (1, 10), (2, 20), (4, 40)",
        ],
        "SELECT id FROM t1 NATURAL JOIN t2",
    );
    // id 1 and 2 match; id 3 has no t2 row; id 4 has no t1 row.
    assert_eq!(rows.len(), 2);
    let ids: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert!(ids.contains(&"1"));
    assert!(ids.contains(&"2"));
}

/// NATURAL LEFT JOIN preserves all rows from the left side.
#[test]
fn natural_left_join() {
    let rows = run(
        SETUP_EMP_DEPT,
        "SELECT ename FROM employees NATURAL LEFT JOIN departments ORDER BY ename",
    );
    assert_eq!(
        rows.len(),
        4,
        "all 4 employees preserved; Dan gets NULL dept"
    );
}

/// NATURAL JOIN on tables with no shared columns → CROSS JOIN (Cartesian product).
#[test]
fn natural_join_disjoint_is_cross() {
    let rows = run(
        &[
            "CREATE TABLE a (x INT)",
            "INSERT INTO a VALUES (1), (2)",
            "CREATE TABLE b (y INT)",
            "INSERT INTO b VALUES (10), (20), (30)",
        ],
        "SELECT a.x, b.y FROM a NATURAL JOIN b",
    );
    // 2 × 3 = 6 rows.
    assert_eq!(rows.len(), 6, "no shared cols → CROSS JOIN");
}

/// NATURAL JOIN with an empty right table yields 0 rows.
#[test]
fn natural_join_empty_right() {
    let rows = run(
        &[
            "CREATE TABLE left_t (id INT, v TEXT)",
            "INSERT INTO left_t VALUES (1, 'a')",
            "CREATE TABLE right_t (id INT, w INT)",
            // right_t is intentionally empty
        ],
        "SELECT id FROM left_t NATURAL JOIN right_t",
    );
    assert_eq!(rows.len(), 0);
}

/// WHERE clause filters correctly after the NATURAL JOIN.
#[test]
fn natural_join_with_where() {
    let rows = run(
        SETUP_EMP_DEPT,
        "SELECT ename FROM employees NATURAL JOIN departments WHERE dname = 'Engineering'",
    );
    assert_eq!(rows.len(), 2, "Alice and Bob are in Engineering");
}

/// Multiple shared columns: NATURAL JOIN matches on ALL of them simultaneously.
/// Only the row with (x=1, y=10) appears in both tables.
#[test]
fn natural_join_multiple_shared_cols() {
    let rows = run(
        &[
            "CREATE TABLE p (x INT, y INT, extra TEXT)",
            "INSERT INTO p VALUES (1, 10, 'p1'), (1, 20, 'p2'), (2, 10, 'p3')",
            "CREATE TABLE q (x INT, y INT, score INT)",
            "INSERT INTO q VALUES (1, 10, 100), (1, 30, 200), (3, 10, 300)",
        ],
        "SELECT x, y FROM p NATURAL JOIN q",
    );
    assert_eq!(rows.len(), 1, "only (x=1, y=10) is in both tables");
    assert_eq!(rows[0][0], "1");
    assert_eq!(rows[0][1], "10");
}
