//! P4.c — subquery / CTE correctness, checked differentially against SQLite
//! on the same data (CLAUDE.md §6): scalar / IN / EXISTS subqueries (correlated
//! and uncorrelated), IN-lists, and WITH CTEs.

use unidb::sql::logical::Literal;
use unidb::{Engine, SqlResult};

fn run_unidb(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let dir = tempfile::tempdir().unwrap();
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

fn run_sqlite(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
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

fn assert_same(setup: &[&str], query: &str) {
    let mut ours = run_unidb(setup, query);
    let mut theirs = run_sqlite(setup, query);
    ours.sort();
    theirs.sort();
    assert_eq!(
        ours, theirs,
        "\nquery: {query}\n unidb: {ours:?}\nsqlite: {theirs:?}"
    );
}

fn shop_setup() -> Vec<&'static str> {
    vec![
        "CREATE TABLE customers (id INT, name TEXT)",
        "CREATE TABLE orders (id INT, customer_id INT, total INT)",
        "INSERT INTO customers (id, name) VALUES (1, 'alice')",
        "INSERT INTO customers (id, name) VALUES (2, 'bob')",
        "INSERT INTO customers (id, name) VALUES (3, 'carol')",
        "INSERT INTO orders (id, customer_id, total) VALUES (10, 1, 100)",
        "INSERT INTO orders (id, customer_id, total) VALUES (11, 1, 250)",
        "INSERT INTO orders (id, customer_id, total) VALUES (12, 2, 75)",
    ]
}

fn sales_setup() -> Vec<&'static str> {
    vec![
        "CREATE TABLE sales (id INT, region TEXT, amount INT)",
        "INSERT INTO sales (id, region, amount) VALUES (1, 'east', 100)",
        "INSERT INTO sales (id, region, amount) VALUES (2, 'east', 200)",
        "INSERT INTO sales (id, region, amount) VALUES (3, 'west', 300)",
        "INSERT INTO sales (id, region, amount) VALUES (4, 'west', 600)",
    ]
}

#[test]
fn uncorrelated_scalar_subquery_in_where() {
    // AVG is exact: 1200/4 = 300.
    assert_same(
        &sales_setup(),
        "SELECT id, amount FROM sales WHERE amount > (SELECT AVG(amount) FROM sales)",
    );
}

#[test]
fn in_subquery_uncorrelated() {
    assert_same(
        &shop_setup(),
        "SELECT name FROM customers WHERE id IN (SELECT customer_id FROM orders)",
    );
}

#[test]
fn not_in_subquery_uncorrelated() {
    assert_same(
        &shop_setup(),
        "SELECT name FROM customers WHERE id NOT IN (SELECT customer_id FROM orders)",
    );
}

#[test]
fn in_list_literal() {
    assert_same(
        &sales_setup(),
        "SELECT id FROM sales WHERE region IN ('east', 'north')",
    );
}

#[test]
fn correlated_exists() {
    assert_same(
        &shop_setup(),
        "SELECT name FROM customers c \
         WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id)",
    );
}

#[test]
fn correlated_not_exists() {
    assert_same(
        &shop_setup(),
        "SELECT name FROM customers c \
         WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id)",
    );
}

#[test]
fn correlated_scalar_subquery_in_projection() {
    assert_same(
        &shop_setup(),
        "SELECT c.name, (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id) \
         FROM customers c",
    );
}

#[test]
fn cte_join() {
    assert_same(
        &shop_setup(),
        "WITH totals AS (SELECT customer_id, SUM(total) AS s FROM orders GROUP BY customer_id) \
         SELECT customers.name, totals.s FROM customers \
         JOIN totals ON customers.id = totals.customer_id",
    );
}

#[test]
fn cte_referenced_twice() {
    // A CTE materialized once and referenced under two aliases (self-join).
    assert_same(
        &shop_setup(),
        "WITH c AS (SELECT id, name FROM customers) \
         SELECT a.name, b.name FROM c a JOIN c b ON a.id = b.id",
    );
}
