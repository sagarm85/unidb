//! Item 19 — IN(subquery) / EXISTS / scalar subquery predicates in WHERE.
//!
//! Tests for subquery predicates in WHERE clauses:
//!   - in_subquery_basic
//!   - not_in_subquery
//!   - in_subquery_empty_set
//!   - exists_subquery_basic
//!   - not_exists_subquery
//!   - scalar_subquery_comparison
//!   - scalar_subquery_null_when_empty
//!   - in_subquery_with_filter
//!   - in_subquery_rls
//!
//! NULL-handling for IN/NOT IN follows SQL three-valued logic:
//!   - `x IN (set with NULLs)` where `x` is not in the set → NULL (not false).
//!   - `x NOT IN (set with NULLs)` → NULL (because one element is unknown).
//!   - If `x` itself is NULL → NULL.

use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

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

/// Run setup statements, execute query, return sorted string rows.
fn run_sorted(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let (engine, _dir) = open();
    let xid = engine.begin().unwrap();
    for stmt in setup {
        engine.execute_sql(xid, stmt).unwrap();
    }
    engine.commit(xid).unwrap();
    let xid = engine.begin().unwrap();
    let results = engine.execute_sql(xid, query).unwrap();
    engine.commit(xid).unwrap();
    let mut rows: Vec<Vec<String>> = results
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
    rows.sort();
    rows
}

/// DDL as superuser (no RLS identity).
fn ddl(engine: &Engine, sql: &str) {
    let x = engine.begin().unwrap();
    engine.execute_sql_as(None, x, sql).unwrap();
    engine.commit(x).unwrap();
}

// ─── IN (subquery) ────────────────────────────────────────────────────────────

/// `WHERE id IN (SELECT user_id FROM orders)` returns the correct set of users.
#[test]
fn in_subquery_basic() {
    let setup = [
        "CREATE TABLE users (id INT, name TEXT)",
        "CREATE TABLE orders (order_id INT, user_id INT)",
        "INSERT INTO users (id, name) VALUES (1, 'alice')",
        "INSERT INTO users (id, name) VALUES (2, 'bob')",
        "INSERT INTO users (id, name) VALUES (3, 'carol')",
        "INSERT INTO orders (order_id, user_id) VALUES (10, 1)",
        "INSERT INTO orders (order_id, user_id) VALUES (11, 2)",
        // user 3 has no order
    ];
    let rows = run_sorted(
        &setup,
        "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders)",
    );
    assert_eq!(rows, vec![vec!["alice"], vec!["bob"]]);
}

/// `WHERE id NOT IN (SELECT id FROM excluded)` returns the complement set.
#[test]
fn not_in_subquery() {
    let setup = [
        "CREATE TABLE items (id INT, label TEXT)",
        "CREATE TABLE excluded (id INT)",
        "INSERT INTO items (id, label) VALUES (1, 'keep')",
        "INSERT INTO items (id, label) VALUES (2, 'drop')",
        "INSERT INTO items (id, label) VALUES (3, 'keep')",
        "INSERT INTO excluded (id) VALUES (2)",
    ];
    let rows = run_sorted(
        &setup,
        "SELECT label FROM items WHERE id NOT IN (SELECT id FROM excluded)",
    );
    assert_eq!(rows, vec![vec!["keep"], vec!["keep"]]);
}

/// `WHERE id IN (SELECT id FROM empty_table)` → 0 rows returned.
#[test]
fn in_subquery_empty_set() {
    let setup = [
        "CREATE TABLE items (id INT, label TEXT)",
        "CREATE TABLE empty_table (id INT)",
        "INSERT INTO items (id, label) VALUES (1, 'a')",
        "INSERT INTO items (id, label) VALUES (2, 'b')",
    ];
    let rows = run_sorted(
        &setup,
        "SELECT label FROM items WHERE id IN (SELECT id FROM empty_table)",
    );
    assert!(rows.is_empty(), "expected 0 rows, got: {rows:?}");
}

/// `WHERE id IN (SELECT user_id FROM orders WHERE total > 150)` — inner subquery
/// with its own WHERE filter.
#[test]
fn in_subquery_with_filter() {
    let setup = [
        "CREATE TABLE customers (id INT, name TEXT)",
        "CREATE TABLE orders (order_id INT, user_id INT, total INT)",
        "INSERT INTO customers (id, name) VALUES (1, 'alice')",
        "INSERT INTO customers (id, name) VALUES (2, 'bob')",
        "INSERT INTO customers (id, name) VALUES (3, 'carol')",
        "INSERT INTO orders (order_id, user_id, total) VALUES (10, 1, 200)",
        "INSERT INTO orders (order_id, user_id, total) VALUES (11, 2, 100)",
        "INSERT INTO orders (order_id, user_id, total) VALUES (12, 3, 50)",
    ];
    // Only alice (user 1) has an order with total > 150.
    let rows = run_sorted(
        &setup,
        "SELECT name FROM customers WHERE id IN (SELECT user_id FROM orders WHERE total > 150)",
    );
    assert_eq!(rows, vec![vec!["alice"]]);
}

// ─── EXISTS (subquery) ────────────────────────────────────────────────────────

/// `WHERE EXISTS (SELECT 1 FROM related WHERE related.fk = t.id)` — correlated EXISTS.
#[test]
fn exists_subquery_basic() {
    let setup = [
        "CREATE TABLE products (id INT, name TEXT)",
        "CREATE TABLE reviews (review_id INT, product_id INT)",
        "INSERT INTO products (id, name) VALUES (1, 'widget')",
        "INSERT INTO products (id, name) VALUES (2, 'gadget')",
        "INSERT INTO products (id, name) VALUES (3, 'doohickey')",
        "INSERT INTO reviews (review_id, product_id) VALUES (100, 1)",
        "INSERT INTO reviews (review_id, product_id) VALUES (101, 1)",
        // product 2 and 3 have no reviews
    ];
    let rows = run_sorted(
        &setup,
        "SELECT name FROM products p \
         WHERE EXISTS (SELECT 1 FROM reviews r WHERE r.product_id = p.id)",
    );
    assert_eq!(rows, vec![vec!["widget"]]);
}

/// `WHERE NOT EXISTS (…)` — complement of EXISTS.
#[test]
fn not_exists_subquery() {
    let setup = [
        "CREATE TABLE products (id INT, name TEXT)",
        "CREATE TABLE reviews (review_id INT, product_id INT)",
        "INSERT INTO products (id, name) VALUES (1, 'widget')",
        "INSERT INTO products (id, name) VALUES (2, 'gadget')",
        "INSERT INTO products (id, name) VALUES (3, 'doohickey')",
        "INSERT INTO reviews (review_id, product_id) VALUES (100, 1)",
        // gadget and doohickey have no reviews → NOT EXISTS matches them
    ];
    let rows = run_sorted(
        &setup,
        "SELECT name FROM products p \
         WHERE NOT EXISTS (SELECT 1 FROM reviews r WHERE r.product_id = p.id)",
    );
    assert_eq!(rows, vec![vec!["doohickey"], vec!["gadget"]]);
}

// ─── Scalar subquery in WHERE ─────────────────────────────────────────────────

/// `WHERE score > (SELECT AVG(score) FROM t)` returns only above-average rows.
#[test]
fn scalar_subquery_comparison() {
    let setup = [
        "CREATE TABLE scores (id INT, score INT)",
        "INSERT INTO scores (id, score) VALUES (1, 10)",
        "INSERT INTO scores (id, score) VALUES (2, 20)",
        "INSERT INTO scores (id, score) VALUES (3, 30)",
        "INSERT INTO scores (id, score) VALUES (4, 40)",
        // AVG = 25, so rows with score > 25 are ids 3 and 4
    ];
    let rows = run_sorted(
        &setup,
        "SELECT id FROM scores WHERE score > (SELECT AVG(score) FROM scores)",
    );
    assert_eq!(rows, vec![vec!["3"], vec!["4"]]);
}

/// Scalar subquery on an empty table yields NULL; comparing to NULL → no rows match.
#[test]
fn scalar_subquery_null_when_empty() {
    let setup = [
        "CREATE TABLE data (id INT, val INT)",
        "CREATE TABLE empty_t (val INT)",
        "INSERT INTO data (id, val) VALUES (1, 5)",
        "INSERT INTO data (id, val) VALUES (2, 10)",
    ];
    // (SELECT MAX(val) FROM empty_t) → NULL; val > NULL → NULL → filtered out.
    let rows = run_sorted(
        &setup,
        "SELECT id FROM data WHERE val > (SELECT MAX(val) FROM empty_t)",
    );
    assert!(
        rows.is_empty(),
        "scalar subquery on empty table should yield NULL, filtering all rows; got: {rows:?}"
    );
}

// ─── RLS inside WHERE subquery ────────────────────────────────────────────────

/// RLS policy applied to the inner subquery of `WHERE id IN (SELECT …)` — the
/// policy must NOT be bypassed by wrapping the query in a subquery.
///
/// Uses `execute_sql` (embedded/superuser path) which applies
/// `apply_rls_skip_current_user`. Literal-predicate policies (no `current_user()`
/// reference) ARE applied by this path — the test verifies they are also applied
/// inside an IN-subquery, not bypassed by the nesting.
#[test]
fn in_subquery_rls() {
    let (engine, _dir) = open();

    // Create tables: documents with an owner column; plus a "visible_docs" policy
    // table whose contents we will test IN (SELECT id FROM docs ...).
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE docs (id INT, owner TEXT, content TEXT)")
        .unwrap();
    engine
        .execute_sql(
            x,
            "CREATE TABLE items (item_id INT, doc_id INT, label TEXT)",
        )
        .unwrap();
    engine
        .execute_sql(
            x,
            "INSERT INTO docs (id, owner, content) VALUES \
             (1, 'alice', 'A1'), (2, 'bob', 'B1'), (3, 'alice', 'A2')",
        )
        .unwrap();
    engine
        .execute_sql(
            x,
            "INSERT INTO items (item_id, doc_id, label) VALUES \
             (10, 1, 'item-for-A1'), (11, 2, 'item-for-B1'), (12, 3, 'item-for-A2')",
        )
        .unwrap();
    engine.commit(x).unwrap();

    // Apply a literal-predicate RLS policy (no current_user()): only rows where
    // owner = 'alice' are visible.
    ddl(
        &engine,
        "CREATE POLICY owner_filter ON docs FOR SELECT USING (owner = 'alice')",
    );

    // Direct query on docs: must see only alice's rows (ids 1 and 3).
    let x = engine.begin().unwrap();
    let direct_results = engine
        .execute_sql(x, "SELECT id FROM docs ORDER BY id")
        .unwrap();
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
        "direct query must see only alice's rows (RLS applied)"
    );

    // IN-subquery: items where doc_id IN (SELECT id FROM docs).
    // RLS on docs must be applied inside the subquery — bob's doc (id 2)
    // must NOT appear in the subquery result, so item-for-B1 must be excluded.
    let x = engine.begin().unwrap();
    let sub_results = engine
        .execute_sql(
            x,
            "SELECT label FROM items WHERE doc_id IN (SELECT id FROM docs)",
        )
        .unwrap();
    engine.commit(x).unwrap();
    let mut sub_rows: Vec<Vec<String>> = sub_results
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
    sub_rows.sort();
    assert_eq!(
        sub_rows,
        vec![vec!["item-for-A1"], vec!["item-for-A2"]],
        "RLS must be applied inside the IN-subquery — item-for-B1 (bob's doc) must be excluded"
    );
}
