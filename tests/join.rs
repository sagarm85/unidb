//! P4.a — join correctness, checked differentially against SQLite on the same
//! data (CLAUDE.md §6: SQLite is the honest reference for the single-engine
//! correctness question). Each case runs identical DDL/DML + a join query
//! against both engines and asserts the result *multisets* match (join output
//! order is unspecified, so both sides are sorted before comparison).

use unidb::sql::logical::Literal;
use unidb::{Engine, SqlResult};

/// Run `setup` statements then `query` on a fresh unidb engine; return rows as
/// canonical strings for order-independent comparison.
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
    let rows = stmt
        .query_map([], |row| {
            let mut v = Vec::with_capacity(ncols);
            for i in 0..ncols {
                let val: rusqlite::types::Value = row.get(i)?;
                v.push(sqlite_to_string(&val));
            }
            Ok(v)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows
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

/// Two customers/orders tables — a classic FK join shape.
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
        // carol (id 3) has no orders; order 13 references a missing customer 9.
        "INSERT INTO orders (id, customer_id, total) VALUES (13, 9, 40)",
    ]
}

#[test]
fn inner_join_matches_sqlite() {
    assert_same(
        &shop_setup(),
        "SELECT customers.name, orders.total FROM customers \
         JOIN orders ON customers.id = orders.customer_id",
    );
}

#[test]
fn left_join_matches_sqlite() {
    // carol must appear with NULL order total.
    assert_same(
        &shop_setup(),
        "SELECT customers.name, orders.total FROM customers \
         LEFT JOIN orders ON customers.id = orders.customer_id",
    );
}

#[test]
fn right_join_matches_sqlite() {
    // order 13 (customer 9) must appear with a NULL customer name.
    assert_same(
        &shop_setup(),
        "SELECT customers.name, orders.total FROM customers \
         RIGHT JOIN orders ON customers.id = orders.customer_id",
    );
}

#[test]
fn inner_join_with_where_filter_matches_sqlite() {
    assert_same(
        &shop_setup(),
        "SELECT customers.name, orders.total FROM customers \
         JOIN orders ON customers.id = orders.customer_id WHERE orders.total > 90",
    );
}

#[test]
fn join_with_nonequi_residual_matches_sqlite() {
    // Equi-key + a non-equi residual on the same ON clause.
    assert_same(
        &shop_setup(),
        "SELECT customers.name, orders.total FROM customers \
         JOIN orders ON customers.id = orders.customer_id AND orders.total > 90",
    );
}

#[test]
fn three_table_join_matches_sqlite() {
    let mut setup = shop_setup();
    setup.push("CREATE TABLE items (order_id INT, sku TEXT)");
    setup.push("INSERT INTO items (order_id, sku) VALUES (10, 'A')");
    setup.push("INSERT INTO items (order_id, sku) VALUES (10, 'B')");
    setup.push("INSERT INTO items (order_id, sku) VALUES (11, 'C')");
    setup.push("INSERT INTO items (order_id, sku) VALUES (12, 'D')");
    assert_same(
        &setup,
        "SELECT customers.name, orders.id, items.sku FROM customers \
         JOIN orders ON customers.id = orders.customer_id \
         JOIN items ON orders.id = items.order_id",
    );
}

#[test]
fn cross_join_matches_sqlite() {
    assert_same(
        &[
            "CREATE TABLE a (x INT)",
            "CREATE TABLE b (y INT)",
            "INSERT INTO a (x) VALUES (1)",
            "INSERT INTO a (x) VALUES (2)",
            "INSERT INTO b (y) VALUES (10)",
            "INSERT INTO b (y) VALUES (20)",
            "INSERT INTO b (y) VALUES (30)",
        ],
        "SELECT a.x, b.y FROM a CROSS JOIN b",
    );
}

#[test]
fn index_nested_loop_join_matches_sqlite() {
    // A B-Tree index on the inner join column makes the planner pick
    // index-nested-loop; results must be identical to SQLite's. The index is a
    // unidb-only DDL (SQLite rejects `USING BTREE`), so it goes into the unidb
    // setup only — it changes the plan, not the answer.
    let query = "SELECT customers.name, orders.total FROM customers \
         JOIN orders ON customers.id = orders.customer_id";
    let base = shop_setup();
    let mut uni_setup = base.clone();
    uni_setup.push("CREATE INDEX idx ON orders USING BTREE (customer_id)");
    let mut ours = run_unidb(&uni_setup, query);
    let mut theirs = run_sqlite(&base, query);
    ours.sort();
    theirs.sort();
    assert_eq!(
        ours, theirs,
        "\nquery: {query}\n unidb: {ours:?}\nsqlite: {theirs:?}"
    );
}

#[test]
fn hash_join_spill_matches_sqlite() {
    // Force Grace spill with a tiny in-memory budget over a larger data set.
    std::env::set_var("UNIDB_HASH_JOIN_MEM_ROWS", "8");
    let mut setup = vec![
        "CREATE TABLE l (k INT, v INT)".to_string(),
        "CREATE TABLE r (k INT, w INT)".to_string(),
    ];
    for i in 0..200 {
        setup.push(format!("INSERT INTO l (k, v) VALUES ({}, {})", i % 40, i));
        setup.push(format!(
            "INSERT INTO r (k, w) VALUES ({}, {})",
            i % 40,
            i * 2
        ));
    }
    let refs: Vec<&str> = setup.iter().map(|s| s.as_str()).collect();
    assert_same(
        &refs,
        "SELECT l.v, r.w FROM l JOIN r ON l.k = r.k WHERE l.v > 100",
    );
    std::env::remove_var("UNIDB_HASH_JOIN_MEM_ROWS");
}
