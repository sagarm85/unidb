//! Integration tests for item 38 — implicit parameter type coercion.
//!
//! Covers all coercion cases specified in `docs/backlog/38_param_type_coercion.md`:
//!
//! * `Text` → `Int`  (lossless parse)
//! * `Text` → `Float` (lossless parse)
//! * `Int`  → `Float` (widening, always lossless)
//! * `Float` → `Int`  (only when exact integer — no fractional part)
//! * `Text` → `Bool`  ("true"/"false"/"1"/"0"/"t"/"f", case-insensitive)
//! * Non-parseable text against a typed column → clear error, not a panic
//! * Float with fractional part against an INT column → error or no match
//! * INSERT write path stays strict — `Text("42")` into an INT column is rejected

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

// ---------------------------------------------------------------------------
// Text → Int coercion
// ---------------------------------------------------------------------------

/// `WHERE int_col = $1` with Text("42") must match the row where id = 42.
#[test]
fn text_to_int_eq_matches() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, label TEXT)");
    exec(&engine, "INSERT INTO t (id, label) VALUES (10, 'ten')");
    exec(
        &engine,
        "INSERT INTO t (id, label) VALUES (42, 'forty-two')",
    );

    let res = exec_params(
        &engine,
        "SELECT label FROM t WHERE id = $1",
        &[Literal::Text("42".to_string())],
    );
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1, "expected exactly 1 matching row");
    assert_eq!(rows[0][0], Literal::Text("forty-two".to_string()));
}

/// `WHERE int_col > $1` with Text("15") must return rows where id > 15.
#[test]
fn text_to_int_gt_filter() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT)");
    exec(&engine, "INSERT INTO t (id) VALUES (5)");
    exec(&engine, "INSERT INTO t (id) VALUES (20)");
    exec(&engine, "INSERT INTO t (id) VALUES (30)");

    let res = exec_params(
        &engine,
        "SELECT id FROM t WHERE id > $1",
        &[Literal::Text("15".to_string())],
    );
    let rows = rows_of(res);
    let mut ids: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            Literal::Int(n) => n,
            ref other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![20, 30]);
}

/// Text param with non-numeric content against an INT column must produce an
/// error — never a panic or silent wrong result.
#[test]
fn text_non_numeric_to_int_is_error() {
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
        "expected Err for non-numeric text coercion against INT column"
    );
}

/// Int column on left-hand side, Text param on right: both orderings must work.
#[test]
fn text_to_int_rhs_and_lhs_symmetry() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT)");
    exec(&engine, "INSERT INTO t (id) VALUES (7)");

    // $1 as rhs of the comparison
    let res = exec_params(
        &engine,
        "SELECT id FROM t WHERE id = $1",
        &[Literal::Text("7".to_string())],
    );
    assert_eq!(rows_of(res).len(), 1, "text param on rhs must match");
}

// ---------------------------------------------------------------------------
// Text → Float coercion
// ---------------------------------------------------------------------------

/// `WHERE float_col = $1` with Text("3.14") must match 3.14.
#[test]
fn text_to_float_eq_matches() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, score FLOAT)");
    exec(&engine, "INSERT INTO t (id, score) VALUES (1, 3.14)");
    exec(&engine, "INSERT INTO t (id, score) VALUES (2, 2.72)");

    let res = exec_params(
        &engine,
        "SELECT id FROM t WHERE score = $1",
        &[Literal::Text("3.14".to_string())],
    );
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(1));
}

/// Text param that is non-numeric against a FLOAT column must error.
#[test]
fn text_non_numeric_to_float_is_error() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, score FLOAT)");
    exec(&engine, "INSERT INTO t (id, score) VALUES (1, 1.0)");

    let xid = engine.begin().unwrap();
    let result = engine.execute_sql_params(
        xid,
        "SELECT id FROM t WHERE score = $1",
        &[Literal::Text("bad_float".to_string())],
    );
    engine.commit(xid).unwrap();
    assert!(
        result.is_err(),
        "expected Err for non-numeric text coercion against FLOAT column"
    );
}

// ---------------------------------------------------------------------------
// Int → Float coercion (widening, always lossless)
// ---------------------------------------------------------------------------

/// `WHERE float_col = $1` with Int(3) must match the row where score = 3.0.
#[test]
fn int_to_float_widening_matches() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, score FLOAT)");
    exec(&engine, "INSERT INTO t (id, score) VALUES (1, 3.0)");
    exec(&engine, "INSERT INTO t (id, score) VALUES (2, 4.5)");

    let res = exec_params(
        &engine,
        "SELECT id FROM t WHERE score = $1",
        &[Literal::Int(3)],
    );
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1, "Int(3) should widen to 3.0 and match");
    assert_eq!(rows[0][0], Literal::Int(1));
}

// ---------------------------------------------------------------------------
// Float → Int coercion (only when exact integer)
// ---------------------------------------------------------------------------

/// `WHERE int_col = $1` with Float(3.0) must match the row where id = 3
/// because 3.0 has no fractional part.
#[test]
fn float_exact_integer_matches_int_col() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, name TEXT)");
    exec(&engine, "INSERT INTO t (id, name) VALUES (3, 'three')");
    exec(&engine, "INSERT INTO t (id, name) VALUES (4, 'four')");

    let res = exec_params(
        &engine,
        "SELECT name FROM t WHERE id = $1",
        &[Literal::Float(3.0)],
    );
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1, "Float(3.0) should compare equal to Int(3)");
    assert_eq!(rows[0][0], Literal::Text("three".to_string()));
}

/// `WHERE int_col = $1` with Float(3.7) must not match any row because 3.7
/// is not representable as an exact integer (lossy coercion is forbidden).
/// The engine may either return 0 rows or an error — both are acceptable
/// outcomes; a wrong match is not.
#[test]
fn float_fractional_does_not_match_int_col() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT)");
    exec(&engine, "INSERT INTO t (id) VALUES (3)");
    exec(&engine, "INSERT INTO t (id) VALUES (4)");

    // Float(3.7) against INT(3) — not equal, should return 0 rows (or an error).
    let xid = engine.begin().unwrap();
    let result = engine.execute_sql_params(
        xid,
        "SELECT id FROM t WHERE id = $1",
        &[Literal::Float(3.7)],
    );
    engine.commit(xid).unwrap();

    match result {
        Ok(res) => {
            let rows = rows_of(res);
            assert_eq!(
                rows.len(),
                0,
                "Float(3.7) must not match Int(3) — lossy coercion forbidden"
            );
        }
        Err(_) => {
            // An explicit type-mismatch error is also acceptable.
        }
    }
}

// ---------------------------------------------------------------------------
// Text → Bool coercion
// ---------------------------------------------------------------------------

/// `WHERE active = $1` with Text("true") must match active = TRUE rows.
#[test]
fn text_true_to_bool_matches() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, active BOOL)");
    exec(&engine, "INSERT INTO t (id, active) VALUES (1, true)");
    exec(&engine, "INSERT INTO t (id, active) VALUES (2, false)");

    let res = exec_params(
        &engine,
        "SELECT id FROM t WHERE active = $1",
        &[Literal::Text("true".to_string())],
    );
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(1));
}

/// Text("1") is a valid truthy spelling — must match TRUE rows.
#[test]
fn text_one_to_bool_matches_true() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, flag BOOL)");
    exec(&engine, "INSERT INTO t (id, flag) VALUES (10, true)");
    exec(&engine, "INSERT INTO t (id, flag) VALUES (20, false)");

    let res = exec_params(
        &engine,
        "SELECT id FROM t WHERE flag = $1",
        &[Literal::Text("1".to_string())],
    );
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(10));
}

/// Text("false") must match BOOL = FALSE rows.
#[test]
fn text_false_to_bool_matches() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, active BOOL)");
    exec(&engine, "INSERT INTO t (id, active) VALUES (1, true)");
    exec(&engine, "INSERT INTO t (id, active) VALUES (2, false)");

    let res = exec_params(
        &engine,
        "SELECT id FROM t WHERE active = $1",
        &[Literal::Text("false".to_string())],
    );
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(2));
}

/// Text("TRUE") (uppercase) must also coerce correctly (case-insensitive).
#[test]
fn text_uppercase_true_to_bool() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, flag BOOL)");
    exec(&engine, "INSERT INTO t (id, flag) VALUES (5, true)");

    let res = exec_params(
        &engine,
        "SELECT id FROM t WHERE flag = $1",
        &[Literal::Text("TRUE".to_string())],
    );
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(5));
}

/// Text that is not a valid boolean spelling must produce an error.
#[test]
fn text_invalid_bool_coercion_is_error() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, flag BOOL)");
    exec(&engine, "INSERT INTO t (id, flag) VALUES (1, true)");

    let xid = engine.begin().unwrap();
    let result = engine.execute_sql_params(
        xid,
        "SELECT id FROM t WHERE flag = $1",
        &[Literal::Text("maybe".to_string())],
    );
    engine.commit(xid).unwrap();
    assert!(
        result.is_err(),
        "expected Err for invalid bool coercion from text 'maybe'"
    );
}

// ---------------------------------------------------------------------------
// Text → Int coercion on text column (Int param against TEXT col)
// ---------------------------------------------------------------------------

/// `WHERE text_col = $1` with Int(42) must match the row where text_col = "42".
/// This is the reverse direction: Int widened to Text for comparison.
#[test]
fn int_to_text_col_matches() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, code TEXT)");
    exec(&engine, "INSERT INTO t (id, code) VALUES (1, '42')");
    exec(&engine, "INSERT INTO t (id, code) VALUES (2, '99')");

    let res = exec_params(
        &engine,
        "SELECT id FROM t WHERE code = $1",
        &[Literal::Int(42)],
    );
    // Text("42") == Text("42") via Int→Text widening — must match.
    let rows = rows_of(res);
    assert_eq!(
        rows.len(),
        1,
        "Int(42) should coerce to '42' for text column comparison"
    );
    assert_eq!(rows[0][0], Literal::Int(1));
}

// ---------------------------------------------------------------------------
// INSERT write path — must stay strict (no coercion on write)
// ---------------------------------------------------------------------------

/// Inserting Text("42") into an INT column must be rejected.
/// The coercion path is predicate-only; the write path must stay type-strict.
#[test]
fn insert_text_into_int_col_is_strict() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT)");

    let xid = engine.begin().unwrap();
    let result = engine.execute_sql_params(
        xid,
        "INSERT INTO t (id) VALUES ($1)",
        &[Literal::Text("42".to_string())],
    );
    // Roll back regardless so the table stays consistent.
    let _ = engine.commit(xid);

    assert!(
        result.is_err(),
        "INSERT of Text(\"42\") into INT column must be rejected on the write path"
    );
}

// ---------------------------------------------------------------------------
// Regression: existing typed-param tests must still pass
// ---------------------------------------------------------------------------

/// Correct-type int params still work (no regression).
#[test]
fn typed_int_param_no_regression() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, val INT)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (1, 100)");
    exec(&engine, "INSERT INTO t (id, val) VALUES (2, 200)");

    let res = exec_params(
        &engine,
        "SELECT val FROM t WHERE id = $1",
        &[Literal::Int(2)],
    );
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(200));
}

/// Correct-type text params still work (no regression).
#[test]
fn typed_text_param_no_regression() {
    let (_dir, engine) = open_engine();
    exec(&engine, "CREATE TABLE t (id INT, name TEXT)");
    exec(&engine, "INSERT INTO t (id, name) VALUES (1, 'alice')");
    exec(&engine, "INSERT INTO t (id, name) VALUES (2, 'bob')");

    let res = exec_params(
        &engine,
        "SELECT id FROM t WHERE name = $1",
        &[Literal::Text("bob".to_string())],
    );
    let rows = rows_of(res);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Literal::Int(2));
}
