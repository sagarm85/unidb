//! E1 (G9) LIKE/ILIKE and E2 (G11) MATCH SQL predicate tests (item 30).
//!
//! LIKE/ILIKE tests are differential against SQLite (rusqlite) to verify
//! identical semantics: wildcards, NULL propagation, case-sensitivity, bound
//! parameters, and both the single-table `LogicalPlan::Select` path and the
//! multi-table `LogicalPlan::Query` (QExpr) path.
//!
//! MATCH tests exercise the over-fetch-then-filter path through an existing
//! FULLTEXT index at the embedded Rust-API level.

use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::{Engine, SqlResult};

// ---------------------------------------------------------------------------
// Helpers shared by LIKE differential tests
// ---------------------------------------------------------------------------

fn run_unidb(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
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
            .map(|r| r.iter().map(lit_to_string).collect())
            .collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// SQLite with `PRAGMA case_sensitive_like = ON` — matches standard SQL LIKE
/// semantics (case-sensitive), since SQLite's default is case-insensitive for
/// ASCII. Use this helper for LIKE differential tests.
fn run_sqlite_cs(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    run_sqlite_inner(setup, query, true)
}

fn run_sqlite_inner(setup: &[&str], query: &str, case_sensitive_like: bool) -> Vec<Vec<String>> {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    if case_sensitive_like {
        conn.execute_batch("PRAGMA case_sensitive_like = ON")
            .unwrap();
    }
    for stmt in setup {
        conn.execute(stmt, []).unwrap();
    }
    let mut stmt = conn.prepare(query).unwrap();
    let ncols = stmt.column_count();
    stmt.query_map([], |row| {
        let mut v = Vec::with_capacity(ncols);
        for i in 0..ncols {
            let val: rusqlite::types::Value = row.get(i)?;
            v.push(sqlite_to_string(&val));
        }
        Ok(v)
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn lit_to_string(l: &Literal) -> String {
    match l {
        Literal::Int(n) => n.to_string(),
        Literal::Text(s) => s.clone(),
        Literal::Bool(b) => (*b as i64).to_string(),
        Literal::Null => "NULL".to_string(),
        Literal::Float(f) => f.to_string(),
        other => format!("{other:?}"),
    }
}

fn sqlite_to_string(v: &rusqlite::types::Value) -> String {
    use rusqlite::types::Value;
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => format!("{b:?}"),
    }
}

/// Differential LIKE test: enable `PRAGMA case_sensitive_like = ON` in SQLite
/// so both sides use standard SQL (case-sensitive) LIKE semantics.
fn assert_like_same(setup: &[&str], query: &str) {
    let mut ours = run_unidb(setup, query);
    let mut theirs = run_sqlite_cs(setup, query);
    ours.sort();
    theirs.sort();
    assert_eq!(
        ours, theirs,
        "\nquery: {query}\n unidb: {ours:?}\nsqlite(cs): {theirs:?}"
    );
}

/// Differential ILIKE test: SQLite has no ILIKE, so we compare unidb's ILIKE
/// against SQLite's `lower(col) LIKE lower(pattern)` equivalent.
fn assert_ilike_same(setup: &[&str], unidb_query: &str, sqlite_equiv: &str) {
    let mut ours = run_unidb(setup, unidb_query);
    let mut theirs = run_sqlite_cs(setup, sqlite_equiv);
    ours.sort();
    theirs.sort();
    assert_eq!(
        ours, theirs,
        "\nunidb : {unidb_query}\nsqlite: {sqlite_equiv}\n unidb: {ours:?}\nsqlite: {theirs:?}"
    );
}

/// Standard dataset: id + name.
fn names_setup() -> Vec<&'static str> {
    vec![
        "CREATE TABLE t (id INT, name TEXT)",
        "INSERT INTO t (id, name) VALUES (1, 'alice')",
        "INSERT INTO t (id, name) VALUES (2, 'bob')",
        "INSERT INTO t (id, name) VALUES (3, 'carol')",
        "INSERT INTO t (id, name) VALUES (4, 'charlie')",
        "INSERT INTO t (id, name) VALUES (5, 'ALICE')",
        "INSERT INTO t (id, name) VALUES (6, 'dave')",
    ]
}

// ---------------------------------------------------------------------------
// E1 — LIKE: percent wildcard (%)
// ---------------------------------------------------------------------------

#[test]
fn like_prefix_percent() {
    // standard SQL LIKE is case-sensitive; use cs helper so SQLite agrees
    assert_like_same(&names_setup(), "SELECT id FROM t WHERE name LIKE 'al%'");
}

#[test]
fn like_suffix_percent() {
    assert_like_same(&names_setup(), "SELECT id FROM t WHERE name LIKE '%e'");
}

#[test]
fn like_infix_percent() {
    assert_like_same(&names_setup(), "SELECT id FROM t WHERE name LIKE '%ar%'");
}

#[test]
fn like_exact_no_wildcard() {
    assert_like_same(&names_setup(), "SELECT id FROM t WHERE name LIKE 'bob'");
}

#[test]
fn like_percent_matches_empty_suffix() {
    // 'alice%' should match 'alice'
    assert_like_same(&names_setup(), "SELECT id FROM t WHERE name LIKE 'alice%'");
}

#[test]
fn like_double_percent() {
    // '%%' must match every non-NULL string
    assert_like_same(&names_setup(), "SELECT id FROM t WHERE name LIKE '%%'");
}

// ---------------------------------------------------------------------------
// E1 — LIKE: underscore wildcard (_)
// ---------------------------------------------------------------------------

#[test]
fn like_underscore_single_char() {
    assert_like_same(&names_setup(), "SELECT id FROM t WHERE name LIKE '_ob'");
}

#[test]
fn like_underscore_prefix() {
    assert_like_same(&names_setup(), "SELECT id FROM t WHERE name LIKE '_lice'");
}

#[test]
fn like_mixed_wildcards() {
    // c_r%  → carol, charlie
    assert_like_same(&names_setup(), "SELECT id FROM t WHERE name LIKE 'c_r%'");
}

// ---------------------------------------------------------------------------
// E1 — NOT LIKE
// ---------------------------------------------------------------------------

#[test]
fn not_like_percent() {
    assert_like_same(&names_setup(), "SELECT id FROM t WHERE name NOT LIKE 'a%'");
}

#[test]
fn not_like_underscore() {
    assert_like_same(&names_setup(), "SELECT id FROM t WHERE name NOT LIKE '_ob'");
}

// ---------------------------------------------------------------------------
// E1 — NULL semantics: NULL LIKE x → NULL (treated as false by WHERE)
// ---------------------------------------------------------------------------

#[test]
fn like_null_lhs_gives_no_rows() {
    // A row with NULL name should not be returned by LIKE.
    let setup = &[
        "CREATE TABLE t (id INT, name TEXT)",
        "INSERT INTO t (id, name) VALUES (1, 'alice')",
        "INSERT INTO t (id, name) VALUES (2, NULL)",
    ];
    assert_like_same(setup, "SELECT id FROM t WHERE name LIKE '%'");
}

#[test]
fn not_like_null_lhs_also_gives_no_row() {
    // NULL NOT LIKE 'x%' → NULL → not returned.
    let setup = &[
        "CREATE TABLE t (id INT, name TEXT)",
        "INSERT INTO t (id, name) VALUES (1, 'alice')",
        "INSERT INTO t (id, name) VALUES (2, NULL)",
    ];
    assert_like_same(setup, "SELECT id FROM t WHERE name NOT LIKE 'z%'");
}

// ---------------------------------------------------------------------------
// E1 — ILIKE: case-insensitive matching
// SQLite has no ILIKE; compare unidb ILIKE against SQLite lower() LIKE lower()
// ---------------------------------------------------------------------------

#[test]
fn ilike_matches_upper_and_lower() {
    // 'alice' ILIKE 'ALICE%' should match both 'alice' (id=1) and 'ALICE' (id=5)
    assert_ilike_same(
        &names_setup(),
        "SELECT id FROM t WHERE name ILIKE 'alice%'",
        "SELECT id FROM t WHERE lower(name) LIKE lower('alice%')",
    );
}

#[test]
fn ilike_prefix_case_insensitive() {
    // 'AL%' ilike should match alice + ALICE
    assert_ilike_same(
        &names_setup(),
        "SELECT id FROM t WHERE name ILIKE 'al%'",
        "SELECT id FROM t WHERE lower(name) LIKE lower('al%')",
    );
}

#[test]
fn not_ilike() {
    assert_ilike_same(
        &names_setup(),
        "SELECT id FROM t WHERE name NOT ILIKE 'a%'",
        "SELECT id FROM t WHERE lower(name) NOT LIKE lower('a%')",
    );
}

// ---------------------------------------------------------------------------
// E1 — LIKE via the QExpr path (JOIN / subquery forces multi-table planner)
// ---------------------------------------------------------------------------

#[test]
fn like_in_join_filter_qexpr_path() {
    // A join forces LogicalPlan::Query (QExpr path), not the fast Select path.
    let setup = &[
        "CREATE TABLE left_t (id INT, name TEXT)",
        "CREATE TABLE right_t (lid INT, val INT)",
        "INSERT INTO left_t (id, name) VALUES (1, 'alice')",
        "INSERT INTO left_t (id, name) VALUES (2, 'bob')",
        "INSERT INTO right_t (lid, val) VALUES (1, 10)",
        "INSERT INTO right_t (lid, val) VALUES (2, 20)",
    ];
    assert_like_same(
        setup,
        "SELECT left_t.id FROM left_t JOIN right_t ON left_t.id = right_t.lid WHERE left_t.name LIKE 'al%'",
    );
}

#[test]
fn ilike_in_join_filter_qexpr_path() {
    let setup = &[
        "CREATE TABLE left_t (id INT, name TEXT)",
        "CREATE TABLE right_t (lid INT, val INT)",
        "INSERT INTO left_t (id, name) VALUES (1, 'alice')",
        "INSERT INTO left_t (id, name) VALUES (2, 'ALICE')",
        "INSERT INTO right_t (lid, val) VALUES (1, 10)",
        "INSERT INTO right_t (lid, val) VALUES (2, 20)",
    ];
    assert_ilike_same(
        setup,
        "SELECT left_t.id FROM left_t JOIN right_t ON left_t.id = right_t.lid WHERE left_t.name ILIKE 'alice'",
        "SELECT left_t.id FROM left_t JOIN right_t ON left_t.id = right_t.lid WHERE lower(left_t.name) LIKE lower('alice')",
    );
}

// ---------------------------------------------------------------------------
// E2 — MATCH: full-text predicate over SQL (G11)
// ---------------------------------------------------------------------------

/// Helper: open a fresh engine with a table + FULLTEXT index + some rows.
fn ft_engine() -> (tempfile::TempDir, Engine) {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE docs (id INT, body TEXT)")
        .unwrap();
    engine
        .execute_sql(xid, "CREATE INDEX ft ON docs USING FULLTEXT (body)")
        .unwrap();
    engine
        .execute_sql(
            xid,
            "INSERT INTO docs (id, body) VALUES (1, 'invoice overdue payment')",
        )
        .unwrap();
    engine
        .execute_sql(
            xid,
            "INSERT INTO docs (id, body) VALUES (2, 'delivery scheduled')",
        )
        .unwrap();
    engine
        .execute_sql(
            xid,
            "INSERT INTO docs (id, body) VALUES (3, 'invoice reminder sent')",
        )
        .unwrap();
    engine.commit(xid).unwrap();
    (dir, engine)
}

fn sql_rows(engine: &Engine, sql: &str) -> Vec<Vec<Literal>> {
    let xid = engine.begin().unwrap();
    let results = engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
    match results.into_iter().next().unwrap() {
        SqlResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn match_single_token_returns_matching_rows() {
    let (_dir, engine) = ft_engine();
    let rows = sql_rows(&engine, "SELECT id FROM docs WHERE MATCH(body, 'invoice')");
    let mut ids: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            Literal::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![1, 3], "both invoice rows must match");
}

#[test]
fn match_two_tokens_and_semantics() {
    // 'invoice overdue' → AND — only row 1 has both tokens
    let (_dir, engine) = ft_engine();
    let rows = sql_rows(
        &engine,
        "SELECT id FROM docs WHERE MATCH(body, 'invoice overdue')",
    );
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            Literal::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    assert_eq!(ids, vec![1], "only doc 1 contains both tokens");
}

#[test]
fn match_no_results_for_absent_token() {
    let (_dir, engine) = ft_engine();
    let rows = sql_rows(
        &engine,
        "SELECT id FROM docs WHERE MATCH(body, 'nonexistent')",
    );
    assert!(rows.is_empty(), "no rows must match an absent token");
}

#[test]
fn match_does_not_return_non_matching_rows() {
    let (_dir, engine) = ft_engine();
    let rows = sql_rows(&engine, "SELECT id FROM docs WHERE MATCH(body, 'delivery')");
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            Literal::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    assert_eq!(ids, vec![2], "only doc 2 has 'delivery'");
}

#[test]
fn match_with_additional_like_predicate() {
    // Combined MATCH + LIKE in a single WHERE should intersect correctly.
    // Only rows that MATCH 'invoice' AND have body LIKE '%overdue%'.
    let (_dir, engine) = ft_engine();
    let rows = sql_rows(
        &engine,
        "SELECT id FROM docs WHERE MATCH(body, 'invoice') AND body LIKE '%overdue%'",
    );
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            Literal::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    assert_eq!(ids, vec![1]);
}
