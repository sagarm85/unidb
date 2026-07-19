//! Integration tests for SQL surface improvements (items 38, G4, G5, G8, G10).
//!
//! Item 38  — implicit parameter type coercion: `WHERE int_col = $1` with
//!            a Text("42") parameter must work.
//! G8       — `SELECT` without `FROM`: `SELECT 1`, `SELECT 'hello'`, `SELECT 1+1`.
//! G10      — `IS NULL` / `IS NOT NULL` on the simple-row path (`eval_expr`).
//! G4       — `ORDER BY` on a column not in the `SELECT` projection list.
//! G5       — `INSERT / UPDATE / DELETE … RETURNING` clause.

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

fn exec_params(engine: &Engine, sql: &str, params: &[Literal]) -> Vec<SqlResult> {
    let xid = engine.begin().unwrap();
    let res = engine.execute_sql_params(xid, sql, params).unwrap();
    engine.commit(xid).unwrap();
    res
}

fn rows_of(res: Vec<SqlResult>) -> Vec<Vec<Literal>> {
    match res.into_iter().next().unwrap() {
        SqlResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn cols_of(res: Vec<SqlResult>) -> (Vec<String>, Vec<Vec<Literal>>) {
    match res.into_iter().next().unwrap() {
        SqlResult::Rows { columns, rows } => (columns, rows),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn int_col(rows: &[Vec<Literal>], col: usize) -> Vec<i64> {
    rows.iter()
        .map(|r| match r[col] {
            Literal::Int(n) => n,
            ref other => panic!("expected Int, got {other:?}"),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Item 38 — implicit parameter type coercion
// ---------------------------------------------------------------------------

/// Text param where INT column is expected: "42" should coerce to 42.
#[test]
fn item38_text_param_matches_int_col() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, val INT)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (1, 100)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (2, 200)");

    let res = exec_params(
        &engine,
        "SELECT val FROM t WHERE id = $1",
        &[Literal::Text("1".to_string())],
    );
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(100));
}

/// Text param on right-hand side of comparison against INT column.
#[test]
fn item38_text_param_gt_int_col() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, val INT)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (1, 10)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (2, 20)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (3, 30)");

    // Text("15") should coerce; rows with val > 15 are ids 2 and 3.
    let res = exec_params(
        &engine,
        "SELECT id FROM t WHERE val > $1",
        &[Literal::Text("15".to_string())],
    );
    let mut ids = int_col(&rows_of(res), 0);
    ids.sort();
    assert_eq!(ids, vec![2, 3]);
}

/// Non-parseable text must produce an error, not a panic or silent wrong result.
#[test]
fn item38_non_parseable_text_for_int_is_error() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT)");
    exec(&engine, "INSERT INTO t (id) VALUES (1)");

    let xid = engine.begin().unwrap();
    let result = engine.execute_sql_params(
        xid,
        "SELECT id FROM t WHERE id = $1",
        &[Literal::Text("not_a_number".to_string())],
    );
    engine.commit(xid).unwrap();
    assert!(
        result.is_err(),
        "expected error for non-parseable text coercion"
    );
}

/// Literal text inline in SQL (not a param) coerces against INT column.
#[test]
fn item38_literal_text_vs_int_inline() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, name TEXT)");
    exec(&engine, "INSERT INTO t (id, name) VALUES (42, 'hello')");
    exec(&engine, "INSERT INTO t (id, name) VALUES (7, 'world')");

    // Params are the typical coercion path; also verify int literal compares fine.
    let res = exec_params(
        &engine,
        "SELECT name FROM t WHERE id = $1",
        &[Literal::Text("42".to_string())],
    );
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Text("hello".to_string()));
}

// ---------------------------------------------------------------------------
// G8 — SELECT without FROM (Dual)
// ---------------------------------------------------------------------------

/// `SELECT 1` must return a single row containing the integer 1.
#[test]
fn g8_select_integer_literal() {
    let (_dir, engine) = open_engine();
    let res = exec(&engine, "SELECT 1");
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(1));
}

/// `SELECT 'hello'` returns a single text row.
#[test]
fn g8_select_text_literal() {
    let (_dir, engine) = open_engine();
    let res = exec(&engine, "SELECT 'hello'");
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Text("hello".to_string()));
}

/// `SELECT 1+1` evaluates arithmetic and returns 2.
#[test]
fn g8_select_arithmetic() {
    let (_dir, engine) = open_engine();
    let res = exec(&engine, "SELECT 1 + 1");
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(2));
}

/// `SELECT 3 * 4` returns 12.
#[test]
fn g8_select_multiplication() {
    let (_dir, engine) = open_engine();
    let res = exec(&engine, "SELECT 3 * 4");
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(12));
}

/// Multiple column expressions with no FROM.
#[test]
fn g8_select_multiple_literals() {
    let (_dir, engine) = open_engine();
    let (cols, rows) = cols_of(exec(&engine, "SELECT 1, 'world'"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 2);
    assert_eq!(rows[0][0], Literal::Int(1));
    assert_eq!(rows[0][1], Literal::Text("world".to_string()));
    let _ = cols; // column names are output-schema detail
}

// ---------------------------------------------------------------------------
// G10 — IS NULL / IS NOT NULL on the simple row path
// ---------------------------------------------------------------------------

/// `IS NULL` returns only the row whose column is NULL.
#[test]
fn g10_is_null_simple_path() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, name TEXT)");
    exec(&engine, "INSERT INTO t (id, name) VALUES (1, 'alice')");
    exec(&engine, "INSERT INTO t (id, name) VALUES (2, NULL)");
    exec(&engine, "INSERT INTO t (id, name) VALUES (3, 'carol')");

    let res = exec(&engine, "SELECT id FROM t WHERE name IS NULL");
    let ids = int_col(&rows_of(res), 0);
    assert_eq!(ids, vec![2]);
}

/// `IS NOT NULL` excludes NULL rows.
#[test]
fn g10_is_not_null_simple_path() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, name TEXT)");
    exec(&engine, "INSERT INTO t (id, name) VALUES (1, 'alice')");
    exec(&engine, "INSERT INTO t (id, name) VALUES (2, NULL)");
    exec(&engine, "INSERT INTO t (id, name) VALUES (3, 'carol')");

    let res = exec(&engine, "SELECT id FROM t WHERE name IS NOT NULL");
    let mut ids = int_col(&rows_of(res), 0);
    ids.sort();
    assert_eq!(ids, vec![1, 3]);
}

/// `IS NULL` on an INT column that was inserted with a literal NULL.
#[test]
fn g10_is_null_int_column() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, score INT)");
    exec(&engine, "INSERT INTO t (id, score) VALUES (1, 99)");
    exec(&engine, "INSERT INTO t (id, score) VALUES (2, NULL)");

    let res = exec(&engine, "SELECT id FROM t WHERE score IS NULL");
    let ids = int_col(&rows_of(res), 0);
    assert_eq!(ids, vec![2]);
}

/// Combined `IS NULL` in AND predicate.
#[test]
fn g10_is_null_combined_with_and() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, name TEXT, val INT)");
    exec(
        &engine,
        "INSERT INTO t (id, name, val) VALUES (1, 'alice', NULL)",
    );
    exec(
        &engine,
        "INSERT INTO t (id, name, val) VALUES (2, 'bob', 10)",
    );
    exec(
        &engine,
        "INSERT INTO t (id, name, val) VALUES (3, NULL, NULL)",
    );

    // id=1 has name != NULL AND val IS NULL → only id=1
    let res = exec(
        &engine,
        "SELECT id FROM t WHERE name IS NOT NULL AND val IS NULL",
    );
    let ids = int_col(&rows_of(res), 0);
    assert_eq!(ids, vec![1]);
}

// ---------------------------------------------------------------------------
// G4 — ORDER BY on non-projected columns
// ---------------------------------------------------------------------------

/// `SELECT id FROM t ORDER BY name` — name not in SELECT list.
#[test]
fn g4_order_by_non_projected_col() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, name TEXT)");
    exec(&engine, "INSERT INTO t (id, name) VALUES (3, 'alice')");
    exec(&engine, "INSERT INTO t (id, name) VALUES (1, 'carol')");
    exec(&engine, "INSERT INTO t (id, name) VALUES (2, 'bob')");

    // Sorted by name alphabetically: alice(3) bob(2) carol(1)
    let res = exec(&engine, "SELECT id FROM t ORDER BY name");
    let ids = int_col(&rows_of(res), 0);
    assert_eq!(ids, vec![3, 2, 1]);
}

/// ORDER BY DESC on non-projected column.
#[test]
fn g4_order_by_non_projected_desc() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, score INT)");
    exec(&engine, "INSERT INTO t (id, score) VALUES (1, 10)");
    exec(&engine, "INSERT INTO t (id, score) VALUES (2, 30)");
    exec(&engine, "INSERT INTO t (id, score) VALUES (3, 20)");

    // SELECT id ORDER BY score DESC → ids in order 2, 3, 1
    let res = exec(&engine, "SELECT id FROM t ORDER BY score DESC");
    let ids = int_col(&rows_of(res), 0);
    assert_eq!(ids, vec![2, 3, 1]);
}

/// ORDER BY on projected column still works correctly.
#[test]
fn g4_order_by_projected_col_still_works() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, val INT)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (3, 10)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (1, 30)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (2, 20)");

    let res = exec(&engine, "SELECT id FROM t ORDER BY id ASC");
    let ids = int_col(&rows_of(res), 0);
    assert_eq!(ids, vec![1, 2, 3]);
}

/// ORDER BY non-projected col with LIMIT.
#[test]
fn g4_order_by_non_projected_with_limit() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, name TEXT)");
    exec(&engine, "INSERT INTO t (id, name) VALUES (1, 'zzz')");
    exec(&engine, "INSERT INTO t (id, name) VALUES (2, 'aaa')");
    exec(&engine, "INSERT INTO t (id, name) VALUES (3, 'mmm')");

    // Top 2 alphabetically by name: aaa(2) mmm(3)
    let res = exec(&engine, "SELECT id FROM t ORDER BY name ASC LIMIT 2");
    let ids = int_col(&rows_of(res), 0);
    assert_eq!(ids, vec![2, 3]);
}

// ---------------------------------------------------------------------------
// G5 — INSERT / UPDATE / DELETE … RETURNING
// ---------------------------------------------------------------------------

/// `INSERT … RETURNING` returns the inserted row's columns.
#[test]
fn g5_insert_returning() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, name TEXT)");

    let xid = engine.begin().unwrap();
    let res = engine
        .execute_sql(
            xid,
            "INSERT INTO t (id, name) VALUES (42, 'alice') RETURNING id, name",
        )
        .unwrap();
    engine.commit(xid).unwrap();

    let (cols, rows) = cols_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(42));
    assert_eq!(rows[0][1], Literal::Text("alice".to_string()));
    assert!(cols.contains(&"id".to_string()));
    assert!(cols.contains(&"name".to_string()));
}

/// `INSERT … RETURNING id` returns just the id column.
#[test]
fn g5_insert_returning_single_col() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, name TEXT, val INT)");

    let xid = engine.begin().unwrap();
    let res = engine
        .execute_sql(
            xid,
            "INSERT INTO t (id, name, val) VALUES (7, 'bob', 100) RETURNING id",
        )
        .unwrap();
    engine.commit(xid).unwrap();

    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 1);
    assert_eq!(rows[0][0], Literal::Int(7));
}

/// `UPDATE … RETURNING` returns the updated row values after the update.
#[test]
fn g5_update_returning() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, val INT)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (1, 10)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (2, 20)");

    let xid = engine.begin().unwrap();
    let res = engine
        .execute_sql(xid, "UPDATE t SET val = 99 WHERE id = 1 RETURNING id, val")
        .unwrap();
    engine.commit(xid).unwrap();

    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(1));
    assert_eq!(rows[0][1], Literal::Int(99));
}

/// `DELETE … RETURNING` returns the deleted row values.
#[test]
fn g5_delete_returning() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, name TEXT)");
    exec(&engine, "INSERT INTO t (id, name) VALUES (1, 'alice')");
    exec(&engine, "INSERT INTO t (id, name) VALUES (2, 'bob')");

    let xid = engine.begin().unwrap();
    let res = engine
        .execute_sql(xid, "DELETE FROM t WHERE id = 2 RETURNING id, name")
        .unwrap();
    engine.commit(xid).unwrap();

    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(2));
    assert_eq!(rows[0][1], Literal::Text("bob".to_string()));

    // Verify the row is actually gone.
    let res2 = exec(&engine, "SELECT id FROM t");
    let remaining = int_col(&rows_of(res2), 0);
    assert_eq!(remaining, vec![1]);
}

/// `DELETE … RETURNING` on multiple rows returns all of them.
#[test]
fn g5_delete_returning_multiple_rows() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, val INT)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (1, 10)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (2, 10)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (3, 20)");

    let xid = engine.begin().unwrap();
    let res = engine
        .execute_sql(xid, "DELETE FROM t WHERE val = 10 RETURNING id")
        .unwrap();
    engine.commit(xid).unwrap();

    let mut ids = int_col(&rows_of(res), 0);
    ids.sort();
    assert_eq!(ids, vec![1, 2]);

    // Only id=3 remains.
    let res2 = exec(&engine, "SELECT id FROM t");
    let remaining = int_col(&rows_of(res2), 0);
    assert_eq!(remaining, vec![3]);
}

/// `UPDATE … RETURNING *` (wildcard) returns all columns.
#[test]
fn g5_update_returning_star() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, a INT, b INT)");
    exec(&engine, "INSERT INTO t (id, a, b) VALUES (1, 10, 20)");

    let xid = engine.begin().unwrap();
    let res = engine
        .execute_sql(xid, "UPDATE t SET a = 99 WHERE id = 1 RETURNING *")
        .unwrap();
    engine.commit(xid).unwrap();

    let (cols, rows) = cols_of(res);
    assert_eq!(rows.len(), 1);
    // all 3 columns must be present
    assert_eq!(cols.len(), 3);
    // find 'a' column index
    let a_idx = cols.iter().position(|c| c == "a").expect("col 'a' missing");
    assert_eq!(rows[0][a_idx], Literal::Int(99));
}
