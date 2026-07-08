//! P4.b — aggregate / GROUP BY / HAVING / ORDER BY / DISTINCT / LIMIT
//! correctness, checked differentially against SQLite on the same data
//! (CLAUDE.md §6). Set-valued queries compare as multisets; ORDER BY / LIMIT
//! queries compare in exact order.

use unidb::sql::logical::Literal;
use unidb::{Engine, SqlResult};

fn run_unidb(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    let xid = engine.begin().unwrap();
    for stmt in setup {
        engine.execute_sql(xid, stmt).unwrap();
    }
    engine.commit(xid).unwrap();
    let xid = engine.begin().unwrap();
    let results = engine.execute_sql(xid, query).unwrap();
    engine.commit(xid).unwrap();
    match results.into_iter().next().unwrap() {
        SqlResult::Rows(rows) => rows
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

/// Order-independent comparison (no ORDER BY).
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

/// Order-sensitive comparison (ORDER BY / LIMIT).
fn assert_same_ordered(setup: &[&str], query: &str) {
    let ours = run_unidb(setup, query);
    let theirs = run_sqlite(setup, query);
    assert_eq!(
        ours, theirs,
        "\nquery: {query}\n unidb: {ours:?}\nsqlite: {theirs:?}"
    );
}

fn sales_setup() -> Vec<&'static str> {
    vec![
        "CREATE TABLE sales (id INT, region TEXT, amount INT)",
        "INSERT INTO sales (id, region, amount) VALUES (1, 'east', 100)",
        "INSERT INTO sales (id, region, amount) VALUES (2, 'east', 200)",
        "INSERT INTO sales (id, region, amount) VALUES (3, 'west', 50)",
        "INSERT INTO sales (id, region, amount) VALUES (4, 'west', 150)",
        "INSERT INTO sales (id, region, amount) VALUES (5, 'west', 400)",
        "INSERT INTO sales (id, region, amount) VALUES (6, 'north', 300)",
    ]
}

#[test]
fn scalar_aggregates_match_sqlite() {
    let s = sales_setup();
    assert_same(&s, "SELECT COUNT(*) FROM sales");
    assert_same(&s, "SELECT COUNT(amount) FROM sales");
    assert_same(&s, "SELECT SUM(amount) FROM sales");
    assert_same(&s, "SELECT MIN(amount), MAX(amount) FROM sales");
    // AVG chosen to be exact (1200/6 = 200).
    assert_same(&s, "SELECT AVG(amount) FROM sales");
}

#[test]
fn group_by_match_sqlite() {
    assert_same(
        &sales_setup(),
        "SELECT region, COUNT(*), SUM(amount) FROM sales GROUP BY region",
    );
}

#[test]
fn group_by_with_where_match_sqlite() {
    assert_same(
        &sales_setup(),
        "SELECT region, SUM(amount) FROM sales WHERE amount > 100 GROUP BY region",
    );
}

#[test]
fn having_match_sqlite() {
    assert_same(
        &sales_setup(),
        "SELECT region, SUM(amount) FROM sales GROUP BY region HAVING SUM(amount) > 250",
    );
}

#[test]
fn count_distinct_match_sqlite() {
    assert_same(&sales_setup(), "SELECT COUNT(DISTINCT region) FROM sales");
}

#[test]
fn distinct_rows_match_sqlite() {
    assert_same(&sales_setup(), "SELECT DISTINCT region FROM sales");
}

#[test]
fn order_by_match_sqlite() {
    assert_same_ordered(
        &sales_setup(),
        "SELECT id, amount FROM sales ORDER BY amount DESC",
    );
    assert_same_ordered(
        &sales_setup(),
        "SELECT id, amount FROM sales ORDER BY amount",
    );
    // ORDER BY output position.
    assert_same_ordered(
        &sales_setup(),
        "SELECT amount, id FROM sales ORDER BY 1 DESC",
    );
}

#[test]
fn order_by_grouped_alias_match_sqlite() {
    assert_same_ordered(
        &sales_setup(),
        // Tie-break on region so equal totals order deterministically in both.
        "SELECT region, SUM(amount) AS total FROM sales GROUP BY region ORDER BY total DESC, region",
    );
}

#[test]
fn limit_offset_match_sqlite() {
    assert_same_ordered(
        &sales_setup(),
        "SELECT id, amount FROM sales ORDER BY amount DESC LIMIT 2",
    );
    assert_same_ordered(
        &sales_setup(),
        "SELECT id, amount FROM sales ORDER BY amount DESC LIMIT 2 OFFSET 1",
    );
}

#[test]
fn aggregate_over_join_match_sqlite() {
    let setup = vec![
        "CREATE TABLE customers (id INT, name TEXT)",
        "CREATE TABLE orders (id INT, customer_id INT, total INT)",
        "INSERT INTO customers (id, name) VALUES (1, 'alice')",
        "INSERT INTO customers (id, name) VALUES (2, 'bob')",
        "INSERT INTO orders (id, customer_id, total) VALUES (10, 1, 100)",
        "INSERT INTO orders (id, customer_id, total) VALUES (11, 1, 250)",
        "INSERT INTO orders (id, customer_id, total) VALUES (12, 2, 75)",
    ];
    assert_same(
        &setup,
        "SELECT customers.name, SUM(orders.total) FROM customers \
         JOIN orders ON customers.id = orders.customer_id GROUP BY customers.name",
    );
}

#[test]
fn external_sort_spill_matches_sqlite() {
    // Force the external merge sort with a tiny in-memory budget.
    std::env::set_var("UNIDB_SORT_MEM_ROWS", "16");
    let mut setup = vec!["CREATE TABLE nums (id INT, v INT)".to_string()];
    for i in 0..300 {
        setup.push(format!(
            "INSERT INTO nums (id, v) VALUES ({}, {})",
            i,
            (i * 7919) % 500
        ));
    }
    let refs: Vec<&str> = setup.iter().map(|s| s.as_str()).collect();
    assert_same_ordered(&refs, "SELECT id, v FROM nums ORDER BY v, id");
    std::env::remove_var("UNIDB_SORT_MEM_ROWS");
}
