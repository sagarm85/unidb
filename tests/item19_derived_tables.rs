//! Item 19 G6 — Derived table (subquery in FROM) tests.
//!
//! Tests for `SELECT … FROM (SELECT …) AS alias` support:
//!   - basic_derived_table
//!   - derived_table_with_filter
//!   - derived_table_count
//!   - derived_table_in_join
//!   - derived_table_alias_column_ref
//!   - nested_derived_table
//!   - derived_table_rls

use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn open() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    (engine, dir)
}

/// Run setup statements, then execute a single query and return all rows as
/// string vectors.
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

fn run_sorted(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let mut rows = run(setup, query);
    rows.sort();
    rows
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

/// Execute DDL as the embedded superuser (no RLS user identity).
fn ddl(engine: &Engine, sql: &str) {
    let x = engine.begin().unwrap();
    engine.execute_sql_as(None, x, sql).unwrap();
    engine.commit(x).unwrap();
}

// ─── G6: basic derived table ─────────────────────────────────────────────────

/// `SELECT x FROM (SELECT id AS x FROM t) sub` returns the correct rows.
#[test]
fn basic_derived_table() {
    let setup = [
        "CREATE TABLE t (id INT)",
        "INSERT INTO t VALUES (1), (2), (3)",
    ];
    let rows = run_sorted(&setup, "SELECT x FROM (SELECT id AS x FROM t) sub");
    assert_eq!(rows, vec![vec!["1"], vec!["2"], vec!["3"]]);
}

/// Outer WHERE applied to derived table results.
#[test]
fn derived_table_with_filter() {
    let setup = [
        "CREATE TABLE t (id INT)",
        "INSERT INTO t VALUES (10), (20), (30), (40)",
    ];
    let rows = run_sorted(
        &setup,
        "SELECT v FROM (SELECT id AS v FROM t) sub WHERE v > 15",
    );
    assert_eq!(rows, vec![vec!["20"], vec!["30"], vec!["40"]]);
}

/// `SELECT cnt FROM (SELECT COUNT(*) AS cnt FROM t) sub` = correct count.
#[test]
fn derived_table_count() {
    let setup = [
        "CREATE TABLE t (id INT)",
        "INSERT INTO t VALUES (1), (2), (3), (4), (5)",
    ];
    let rows = run(
        &setup,
        "SELECT cnt FROM (SELECT COUNT(*) AS cnt FROM t) sub",
    );
    assert_eq!(rows, vec![vec!["5"]]);
}

/// `SELECT a.id FROM t1 a JOIN (SELECT id FROM t2 WHERE active = 1) b ON a.id = b.id`
#[test]
fn derived_table_in_join() {
    let setup = [
        "CREATE TABLE t1 (id INT)",
        "CREATE TABLE t2 (id INT, active INT)",
        "INSERT INTO t1 VALUES (1), (2), (3)",
        "INSERT INTO t2 VALUES (1, 1), (2, 0), (3, 1)",
    ];
    // Only rows in t2 where active = 1 (id 1 and 3) join with t1.
    let rows = run_sorted(
        &setup,
        "SELECT a.id FROM t1 a JOIN (SELECT id FROM t2 WHERE active = 1) b ON a.id = b.id",
    );
    assert_eq!(rows, vec![vec!["1"], vec!["3"]]);
}

/// Column references of the form `alias.col` work for derived tables.
#[test]
fn derived_table_alias_column_ref() {
    let setup = [
        "CREATE TABLE t (name TEXT)",
        "INSERT INTO t VALUES ('alice'), ('bob')",
    ];
    let rows = run_sorted(&setup, "SELECT sub.name FROM (SELECT name FROM t) sub");
    assert_eq!(rows, vec![vec!["alice"], vec!["bob"]]);
}

/// Two levels of nesting: `SELECT x FROM (SELECT y AS x FROM (SELECT id AS y FROM t) inner) outer`
#[test]
fn nested_derived_table() {
    let setup = [
        "CREATE TABLE t (id INT)",
        "INSERT INTO t VALUES (7), (8), (9)",
    ];
    let rows = run_sorted(
        &setup,
        "SELECT x FROM (SELECT y AS x FROM (SELECT id AS y FROM t) inner_sub) outer_sub",
    );
    assert_eq!(rows, vec![vec!["7"], vec!["8"], vec!["9"]]);
}

/// RLS policy applied to inner subquery correctly (not bypassed by nesting).
///
/// Uses `execute_sql` (embedded/superuser path) which applies
/// `apply_rls_skip_current_user`. Literal-predicate policies (no `current_user()`
/// reference) ARE applied by this path — the test verifies they are also applied
/// inside a derived table's inner subquery, not bypassed by the nesting.
#[test]
fn derived_table_rls() {
    let (engine, _dir) = open();

    // Set up table and rows.
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE docs (id INT, owner TEXT, content TEXT)")
        .unwrap();
    engine
        .execute_sql(
            x,
            "INSERT INTO docs VALUES (1, 'alice', 'A1'), (2, 'bob', 'B1'), (3, 'alice', 'A2')",
        )
        .unwrap();
    engine.commit(x).unwrap();

    // Apply a literal-predicate RLS policy (no current_user()): only rows where
    // owner = 'alice' are visible. A literal policy is applied by
    // apply_rls_skip_current_user (the embedded path), so we can test via
    // execute_sql without needing a named user.
    ddl(
        &engine,
        "CREATE POLICY owner_filter ON docs FOR SELECT USING (owner = 'alice')",
    );

    // Direct query sees only alice's rows (RLS via apply_rls_skip_current_user).
    let x = engine.begin().unwrap();
    let direct_results = engine.execute_sql(x, "SELECT id FROM docs").unwrap();
    engine.commit(x).unwrap();
    let mut direct: Vec<Vec<String>> = direct_results
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
        .collect();
    direct.sort();
    assert_eq!(
        direct,
        vec![vec!["1"], vec!["3"]],
        "direct query: RLS policy must filter to alice's rows only"
    );

    // Same query wrapped in a derived table — RLS must NOT be bypassed.
    let x = engine.begin().unwrap();
    let derived_results = engine
        .execute_sql(x, "SELECT doc_id FROM (SELECT id AS doc_id FROM docs) sub")
        .unwrap();
    engine.commit(x).unwrap();
    let mut derived: Vec<Vec<String>> = derived_results
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
        .collect();
    derived.sort();
    assert_eq!(
        derived,
        vec![vec!["1"], vec!["3"]],
        "RLS must be applied inside the derived table subquery, not bypassed by nesting"
    );
}
