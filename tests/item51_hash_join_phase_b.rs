//! Item 51 Phase B — in-memory hash join for equi-joins.
//!
//! Tests verify: correctness of hash-join path in INLJ, fallback to INLJ when
//! budget is exceeded, NULL key semantics, duplicate inner keys, and that the
//! hash-join path is faster than the forced-INLJ path on a non-trivial join.

use std::time::Instant;
use unidb::sql::logical::Literal;
use unidb::{Engine, SqlResult};

// ── helpers ──────────────────────────────────────────────────────────────────

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
            v.push(match &val {
                rusqlite::types::Value::Null => "NULL".to_string(),
                rusqlite::types::Value::Integer(n) => n.to_string(),
                rusqlite::types::Value::Real(f) => f.to_string(),
                rusqlite::types::Value::Text(s) => s.clone(),
                rusqlite::types::Value::Blob(b) => format!("{b:?}"),
            });
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

/// Run a join query against a fresh engine and return rows + elapsed seconds.
fn run_timed(setup: &[&str], query: &str) -> (Vec<Vec<String>>, f64) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let xid = engine.begin().unwrap();
    for stmt in setup {
        engine.execute_sql(xid, stmt).unwrap();
    }
    engine.commit(xid).unwrap();

    let xid = engine.begin().unwrap();
    let t0 = Instant::now();
    let results = engine.execute_sql(xid, query).unwrap();
    let elapsed = t0.elapsed().as_secs_f64();
    engine.commit(xid).unwrap();
    let rows = match results.into_iter().next().unwrap() {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| r.iter().map(lit_to_string).collect())
            .collect(),
        other => panic!("expected Rows, got {other:?}"),
    };
    (rows, elapsed)
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Basic correctness: 1k orders × 100 customers, hash-join path (inner fits budget).
/// Verifies against SQLite reference — same rows, same column values.
#[test]
fn hash_join_basic_correctness() {
    let n_customers = 100u32;
    let n_orders = 1_000u32;

    // unidb setup: customers has PRIMARY KEY → triggers INLJ planner, Phase B
    // intercepts with hash table (inner row count well under budget).
    let mut uni_setup = vec![
        "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT)".to_string(),
        "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT REFERENCES customers(id), amount INT)".to_string(),
    ];
    for i in 0..n_customers {
        uni_setup.push(format!(
            "INSERT INTO customers (id, name) VALUES ({i}, 'cust{i}')"
        ));
    }
    for i in 0..n_orders {
        let cid = i % n_customers;
        uni_setup.push(format!(
            "INSERT INTO orders (id, customer_id, amount) VALUES ({i}, {cid}, {})",
            (i * 7) % 1000
        ));
    }

    // SQLite setup: no PK/FK enforcement, plain INT columns
    let mut sqlite_setup = vec![
        "CREATE TABLE customers (id INT, name TEXT)".to_string(),
        "CREATE TABLE orders (id INT, customer_id INT, amount INT)".to_string(),
    ];
    for i in 0..n_customers {
        sqlite_setup.push(format!(
            "INSERT INTO customers (id, name) VALUES ({i}, 'cust{i}')"
        ));
    }
    for i in 0..n_orders {
        let cid = i % n_customers;
        sqlite_setup.push(format!(
            "INSERT INTO orders (id, customer_id, amount) VALUES ({i}, {cid}, {})",
            (i * 7) % 1000
        ));
    }

    let query = "SELECT orders.id, customers.name, orders.amount \
                 FROM orders JOIN customers ON orders.customer_id = customers.id \
                 WHERE orders.amount > 500";

    let uni_refs: Vec<&str> = uni_setup.iter().map(|s| s.as_str()).collect();
    let sqlite_refs: Vec<&str> = sqlite_setup.iter().map(|s| s.as_str()).collect();

    let mut ours = run_unidb(&uni_refs, query);
    let mut theirs = run_sqlite(&sqlite_refs, query);
    ours.sort();
    theirs.sort();
    assert_eq!(
        ours, theirs,
        "\nhash_join_basic: row sets differ\n unidb: {ours:?}\nsqlite: {theirs:?}"
    );
}

/// Fallback to INLJ: set budget to 1 row, forcing the B-tree path.
/// Result must still be correct (same as SQLite).
#[test]
fn hash_join_fallback_to_inlj_when_budget_exceeded() {
    // Force INLJ by setting budget to 1 row (inner has 5 customers).
    std::env::set_var("UNIDB_HASH_JOIN_BUDGET", "1");

    let setup = vec![
        "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT)",
        "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT REFERENCES customers(id), total INT)",
        "INSERT INTO customers (id, name) VALUES (1, 'alice')",
        "INSERT INTO customers (id, name) VALUES (2, 'bob')",
        "INSERT INTO customers (id, name) VALUES (3, 'carol')",
        "INSERT INTO orders (id, customer_id, total) VALUES (10, 1, 100)",
        "INSERT INTO orders (id, customer_id, total) VALUES (11, 2, 200)",
        "INSERT INTO orders (id, customer_id, total) VALUES (12, 1, 300)",
    ];
    let sqlite_setup = vec![
        "CREATE TABLE customers (id INT, name TEXT)",
        "CREATE TABLE orders (id INT, customer_id INT, total INT)",
        "INSERT INTO customers (id, name) VALUES (1, 'alice')",
        "INSERT INTO customers (id, name) VALUES (2, 'bob')",
        "INSERT INTO customers (id, name) VALUES (3, 'carol')",
        "INSERT INTO orders (id, customer_id, total) VALUES (10, 1, 100)",
        "INSERT INTO orders (id, customer_id, total) VALUES (11, 2, 200)",
        "INSERT INTO orders (id, customer_id, total) VALUES (12, 1, 300)",
    ];
    let query = "SELECT customers.name, orders.total \
                 FROM orders JOIN customers ON orders.customer_id = customers.id";

    let mut ours = run_unidb(&setup, query);
    let mut theirs = run_sqlite(&sqlite_setup, query);
    ours.sort();
    theirs.sort();
    assert_eq!(
        ours, theirs,
        "\nfallback test: row sets differ\n unidb: {ours:?}\nsqlite: {theirs:?}"
    );

    std::env::remove_var("UNIDB_HASH_JOIN_BUDGET");
}

/// NULL keys must not match — SQL equi-join semantics.
/// A NULL customer_id on an order must not join to any customer.
#[test]
fn hash_join_null_keys_do_not_match() {
    let uni_setup = vec![
        "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT)",
        "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT, total INT)",
        "INSERT INTO customers (id, name) VALUES (1, 'alice')",
        "INSERT INTO customers (id, name) VALUES (2, 'bob')",
        "INSERT INTO orders (id, customer_id, total) VALUES (10, 1, 100)",
        "INSERT INTO orders (id, customer_id, total) VALUES (11, NULL, 200)", // NULL fk
        "INSERT INTO orders (id, customer_id, total) VALUES (12, 2, 300)",
    ];
    let sqlite_setup = vec![
        "CREATE TABLE customers (id INT, name TEXT)",
        "CREATE TABLE orders (id INT, customer_id INT, total INT)",
        "INSERT INTO customers (id, name) VALUES (1, 'alice')",
        "INSERT INTO customers (id, name) VALUES (2, 'bob')",
        "INSERT INTO orders (id, customer_id, total) VALUES (10, 1, 100)",
        "INSERT INTO orders (id, customer_id, total) VALUES (11, NULL, 200)",
        "INSERT INTO orders (id, customer_id, total) VALUES (12, 2, 300)",
    ];
    // INNER JOIN: order 11 (NULL customer_id) must not appear.
    let query = "SELECT orders.id, customers.name \
                 FROM orders JOIN customers ON orders.customer_id = customers.id \
                 ORDER BY orders.id";

    let mut ours = run_unidb(&uni_setup, query);
    let mut theirs = run_sqlite(&sqlite_setup, query);
    ours.sort();
    theirs.sort();
    assert_eq!(
        ours, theirs,
        "\nnull key test: row sets differ\n unidb: {ours:?}\nsqlite: {theirs:?}"
    );
    // Extra: order 11 must not be in the result.
    let has_null_order = ours.iter().any(|r| r[0] == "11");
    assert!(
        !has_null_order,
        "NULL customer_id order should not appear in INNER JOIN result"
    );
}

/// Duplicate inner keys: inner relation has non-unique join keys (no PK on join col).
/// All matching outer/inner pairs must be returned.
#[test]
fn hash_join_duplicate_inner_keys_all_matches_returned() {
    // products.category has duplicates; order_items.category_id joins to it.
    let setup = vec![
        "CREATE TABLE products (id INT, category INT, name TEXT)",
        "CREATE TABLE order_items (oid INT, category_id INT, qty INT)",
        "INSERT INTO products (id, category, name) VALUES (1, 10, 'apple')",
        "INSERT INTO products (id, category, name) VALUES (2, 10, 'banana')", // same cat
        "INSERT INTO products (id, category, name) VALUES (3, 20, 'carrot')",
        "INSERT INTO order_items (oid, category_id, qty) VALUES (100, 10, 5)",
        "INSERT INTO order_items (oid, category_id, qty) VALUES (101, 20, 3)",
    ];
    let query = "SELECT order_items.oid, products.name \
                 FROM order_items JOIN products ON order_items.category_id = products.category";

    // Both apple and banana must appear paired with oid=100.
    assert_same(&setup, query);

    let mut rows = run_unidb(&setup, query);
    rows.sort();
    // oid 100 pairs with both apple and banana → 2 rows; oid 101 → 1 row.
    assert_eq!(rows.len(), 3, "expected 3 result rows, got: {rows:?}");
    let oid100_matches: Vec<_> = rows.iter().filter(|r| r[0] == "100").collect();
    assert_eq!(
        oid100_matches.len(),
        2,
        "oid 100 must match both products in category 10, got: {oid100_matches:?}"
    );
}

/// Hash join returns the same rows as forced-INLJ, and is at least as fast
/// on 10k outer × 1k inner rows (release-mode skew assertion; debug just
/// checks correctness to avoid timing flakiness).
#[test]
fn hash_join_performance_better_than_inlj() {
    let n_customers = 1_000u32;
    let n_orders = 10_000u32;

    let mut setup_stmts = vec![
        "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT)".to_string(),
        "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT REFERENCES customers(id), amount INT)".to_string(),
    ];
    for i in 0..n_customers {
        setup_stmts.push(format!(
            "INSERT INTO customers (id, name) VALUES ({i}, 'c{i}')"
        ));
    }
    for i in 0..n_orders {
        let cid = i % n_customers;
        setup_stmts.push(format!(
            "INSERT INTO orders (id, customer_id, amount) VALUES ({i}, {cid}, {})",
            (i * 3) % 500
        ));
    }

    let query = "SELECT orders.id, customers.name \
                 FROM orders JOIN customers ON orders.customer_id = customers.id";

    let setup_refs: Vec<&str> = setup_stmts.iter().map(|s| s.as_str()).collect();

    // Hash join path (default budget = 500k, inner has 1000 rows → uses hash table).
    std::env::remove_var("UNIDB_HASH_JOIN_BUDGET");
    let (hash_rows, hash_secs) = run_timed(&setup_refs, query);

    // Force INLJ fallback by setting budget to 1.
    std::env::set_var("UNIDB_HASH_JOIN_BUDGET", "1");
    let (inlj_rows, inlj_secs) = run_timed(&setup_refs, query);
    std::env::remove_var("UNIDB_HASH_JOIN_BUDGET");

    // Both paths must return the same rows (primary correctness assertion).
    let mut h = hash_rows;
    let mut i = inlj_rows;
    h.sort();
    i.sort();
    assert_eq!(h, i, "hash-join and INLJ paths must return identical rows");
    assert_eq!(
        h.len(),
        n_orders as usize,
        "expected {n_orders} result rows"
    );

    // Performance comparison: only assert in release builds (debug is too slow
    // and noisy to give a stable ratio). The real speedup is measured in the
    // Docker bench report; here we just gate on a very conservative threshold
    // in release mode to catch regressions.
    if cfg!(not(debug_assertions)) && inlj_secs > 0.005 {
        let ratio = inlj_secs / hash_secs;
        assert!(
            ratio >= 1.5,
            "hash join ({hash_secs:.4}s) should be ≥1.5× faster than INLJ ({inlj_secs:.4}s), ratio={ratio:.2}"
        );
    }
}

/// Left join: unmatched outer rows must still appear with NULLs on right side.
#[test]
fn hash_join_left_join_unmatched_outer_rows() {
    let uni_setup = vec![
        "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT)",
        "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT REFERENCES customers(id), total INT)",
        "INSERT INTO customers (id, name) VALUES (1, 'alice')",
        "INSERT INTO customers (id, name) VALUES (2, 'bob')",   // bob has no orders
        "INSERT INTO orders (id, customer_id, total) VALUES (10, 1, 100)",
        "INSERT INTO orders (id, customer_id, total) VALUES (11, 1, 200)",
    ];
    let sqlite_setup = vec![
        "CREATE TABLE customers (id INT, name TEXT)",
        "CREATE TABLE orders (id INT, customer_id INT, total INT)",
        "INSERT INTO customers (id, name) VALUES (1, 'alice')",
        "INSERT INTO customers (id, name) VALUES (2, 'bob')",
        "INSERT INTO orders (id, customer_id, total) VALUES (10, 1, 100)",
        "INSERT INTO orders (id, customer_id, total) VALUES (11, 1, 200)",
    ];
    let query = "SELECT customers.name, orders.total \
                 FROM customers LEFT JOIN orders ON customers.id = orders.customer_id";

    let mut ours = run_unidb(&uni_setup, query);
    let mut theirs = run_sqlite(&sqlite_setup, query);
    ours.sort();
    theirs.sort();
    assert_eq!(
        ours, theirs,
        "\nleft-join test: row sets differ\n unidb: {ours:?}\nsqlite: {theirs:?}"
    );
    // bob must appear with NULL total.
    let bob_null = ours.iter().any(|r| r[0] == "bob" && r[1] == "NULL");
    assert!(
        bob_null,
        "bob should appear with NULL total in LEFT JOIN: {ours:?}"
    );
}
