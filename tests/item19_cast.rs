//! Item 19 G2 — CAST expression tests.
//!
//! Covers `CAST(expr AS type)` across the practical subset of type pairs:
//! any → TEXT, TEXT → INT, FLOAT → INT (truncate), INT → FLOAT,
//! BOOL → TEXT, INT → BOOL, NULL → any, CAST in WHERE clause.

use unidb::sql::logical::Literal;
use unidb::{Engine, SqlResult};

// ─── helpers ─────────────────────────────────────────────────────────────────

fn open() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    (engine, dir)
}

/// Run setup statements then a single query; return rows as string vecs.
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

/// Like `run` but returns an error result from the query.
fn run_err(setup: &[&str], query: &str) -> String {
    let (engine, _dir) = open();
    let xid = engine.begin().unwrap();
    for stmt in setup {
        engine.execute_sql(xid, stmt).unwrap();
    }
    engine.commit(xid).unwrap();
    let xid = engine.begin().unwrap();
    let err = engine.execute_sql(xid, query).unwrap_err();
    let _ = engine.abort(xid);
    err.to_string()
}

fn lit_to_str(l: &Literal) -> String {
    match l {
        Literal::Int(n) => n.to_string(),
        Literal::Text(s) => s.clone(),
        Literal::Bool(b) => b.to_string(),
        Literal::Null => "NULL".to_string(),
        Literal::Float(f) => f.to_string(),
        other => format!("{other:?}"),
    }
}

// ─── G2: CAST ────────────────────────────────────────────────────────────────

/// CAST(int_col AS TEXT) should produce the string representation of each id.
#[test]
fn cast_int_to_text() {
    let setup = [
        "CREATE TABLE t (id INT)",
        "INSERT INTO t VALUES (1)",
        "INSERT INTO t VALUES (42)",
        "INSERT INTO t VALUES (-7)",
    ];
    let mut rows = run(&setup, "SELECT CAST(id AS TEXT) FROM t");
    rows.sort();
    assert_eq!(rows, vec![vec!["-7"], vec!["1"], vec!["42"]]);
}

/// CAST('42' AS INT) over a literal should return the integer 42.
#[test]
fn cast_text_to_int() {
    let rows = run(&[], "SELECT CAST('42' AS INT)");
    assert_eq!(rows, vec![vec!["42"]]);
}

/// CAST a text column value to INT.
#[test]
fn cast_text_col_to_int() {
    let setup = [
        "CREATE TABLE t (label TEXT)",
        "INSERT INTO t VALUES ('10')",
        "INSERT INTO t VALUES ('200')",
    ];
    let mut rows = run(&setup, "SELECT CAST(label AS INT) FROM t");
    rows.sort();
    assert_eq!(rows, vec![vec!["10"], vec!["200"]]);
}

/// CAST('abc' AS INT) should return an error (not panic).
#[test]
fn cast_text_invalid_to_int_errors() {
    let msg = run_err(&[], "SELECT CAST('abc' AS INT)");
    assert!(
        msg.contains("CAST") || msg.contains("cannot convert"),
        "unexpected error message: {msg}"
    );
}

/// CAST(3.9 AS INT) should truncate toward zero → 3.
#[test]
fn cast_float_to_int_truncates() {
    let rows = run(&[], "SELECT CAST(3.9 AS INT)");
    assert_eq!(rows, vec![vec!["3"]]);
}

/// CAST(-2.7 AS INT) should truncate toward zero → -2.
#[test]
fn cast_float_negative_to_int_truncates() {
    let rows = run(&[], "SELECT CAST(-2.7 AS INT)");
    assert_eq!(rows, vec![vec!["-2"]]);
}

/// CAST(3 AS FLOAT) should produce a float.
#[test]
fn cast_int_to_float() {
    let rows = run(&[], "SELECT CAST(3 AS FLOAT)");
    // The rendered form is "3" (i64 → f64 → Display). Accept "3" or "3.0".
    assert!(
        rows == vec![vec!["3"]] || rows == vec![vec!["3.0"]],
        "unexpected rows: {rows:?}"
    );
}

/// CAST(true AS TEXT) should produce the text "true".
#[test]
fn cast_bool_to_text() {
    let rows = run(&[], "SELECT CAST(true AS TEXT)");
    assert_eq!(rows, vec![vec!["true"]]);
}

/// CAST(false AS TEXT) should produce the text "false".
#[test]
fn cast_bool_false_to_text() {
    let rows = run(&[], "SELECT CAST(false AS TEXT)");
    assert_eq!(rows, vec![vec!["false"]]);
}

/// CAST(NULL AS INT) should still be NULL.
#[test]
fn cast_null_is_null() {
    let rows = run(&[], "SELECT CAST(NULL AS INT)");
    assert_eq!(rows, vec![vec!["NULL"]]);
}

/// CAST(NULL AS TEXT) should still be NULL.
#[test]
fn cast_null_to_text_is_null() {
    let rows = run(&[], "SELECT CAST(NULL AS TEXT)");
    assert_eq!(rows, vec![vec!["NULL"]]);
}

/// CAST(NULL AS FLOAT) should still be NULL.
#[test]
fn cast_null_to_float_is_null() {
    let rows = run(&[], "SELECT CAST(NULL AS FLOAT)");
    assert_eq!(rows, vec![vec!["NULL"]]);
}

/// WHERE CAST(label AS INT) > 5 — CAST in predicate filters correctly.
#[test]
fn cast_in_where_clause() {
    let setup = [
        "CREATE TABLE t (label TEXT)",
        "INSERT INTO t VALUES ('3')",
        "INSERT INTO t VALUES ('7')",
        "INSERT INTO t VALUES ('10')",
        "INSERT INTO t VALUES ('2')",
    ];
    let mut rows = run(&setup, "SELECT label FROM t WHERE CAST(label AS INT) > 5");
    rows.sort();
    assert_eq!(rows, vec![vec!["10"], vec!["7"]]);
}

/// Combined: CAST in both SELECT list and WHERE clause.
#[test]
fn cast_in_select_and_where() {
    let setup = [
        "CREATE TABLE t (id INT, name TEXT)",
        "INSERT INTO t VALUES (1, 'alice')",
        "INSERT INTO t VALUES (2, 'bob')",
        "INSERT INTO t VALUES (3, 'carol')",
    ];
    // CAST(id AS TEXT) in SELECT, CAST(id AS TEXT) != '2' in WHERE.
    let mut rows = run(
        &setup,
        "SELECT CAST(id AS TEXT), name FROM t WHERE CAST(id AS TEXT) != '2'",
    );
    rows.sort();
    assert_eq!(rows, vec![vec!["1", "alice"], vec!["3", "carol"]]);
}

/// CAST arithmetic: CAST('42' AS INT) + 1 should equal 43.
#[test]
fn cast_text_to_int_arithmetic() {
    let rows = run(&[], "SELECT CAST('42' AS INT) + 1");
    assert_eq!(rows, vec![vec!["43"]]);
}

/// CAST(score FLOAT col AS BIGINT) — exercises the column path.
#[test]
fn cast_float_col_to_int() {
    let setup = [
        "CREATE TABLE t (score FLOAT)",
        "INSERT INTO t VALUES (9.8)",
        "INSERT INTO t VALUES (3.1)",
    ];
    let mut rows = run(&setup, "SELECT CAST(score AS BIGINT) FROM t");
    rows.sort();
    assert_eq!(rows, vec![vec!["3"], vec!["9"]]);
}

/// CAST(active BOOL col AS TEXT) — bool column to text.
#[test]
fn cast_bool_col_to_text() {
    let setup = [
        "CREATE TABLE t (active BOOLEAN)",
        "INSERT INTO t VALUES (true)",
        "INSERT INTO t VALUES (false)",
    ];
    let mut rows = run(&setup, "SELECT CAST(active AS TEXT) FROM t");
    rows.sort();
    assert_eq!(rows, vec![vec!["false"], vec!["true"]]);
}

/// CAST to an unsupported type (TIMESTAMP) returns a parse-level error.
#[test]
fn cast_to_unsupported_type_errors() {
    let msg = run_err(&[], "SELECT CAST('2024-01-01' AS TIMESTAMP)");
    assert!(
        msg.contains("CAST") || msg.contains("not supported") || msg.contains("unsupported"),
        "unexpected error message: {msg}"
    );
}
