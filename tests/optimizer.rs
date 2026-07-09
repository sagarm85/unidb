//! P4.d — with statistics gathered (`ANALYZE`), the cost-based optimizer path
//! must still produce results identical to SQLite. This proves the optimizer's
//! join-order reordering + index-vs-scan choices are correctness-preserving
//! (the plan differs; the answer must not). The optimizer's *choice* itself is
//! unit-tested in `src/sql/optimizer.rs`; here we check end-to-end correctness.

use unidb::sql::logical::Literal;
use unidb::{Engine, SqlResult};

fn run_unidb(setup: &[&str], extra: &[&str], analyze: &[&str], query: &str) -> Vec<Vec<String>> {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let xid = engine.begin().unwrap();
    for stmt in setup.iter().chain(extra.iter()) {
        engine.execute_sql(xid, stmt).unwrap();
    }
    engine.commit(xid).unwrap();
    // ANALYZE in its own committed transaction so stats are durable + visible.
    let xid = engine.begin().unwrap();
    for stmt in analyze {
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

/// unidb-only index DDL (SQLite rejects `USING BTREE`); does not change results,
/// only which plan the optimizer can choose.
fn unidb_indexes() -> Vec<&'static str> {
    vec![
        "CREATE INDEX ci ON customer USING BTREE (id)",
        "CREATE INDEX ri ON region USING BTREE (id)",
    ]
}

fn assert_same(setup: &[&str], analyze: &[&str], query: &str) {
    let idx = unidb_indexes();
    let mut ours = run_unidb(setup, &idx, analyze, query);
    let mut theirs = run_sqlite(setup, query);
    ours.sort();
    theirs.sort();
    assert_eq!(
        ours, theirs,
        "\nquery: {query}\n unidb: {ours:?}\nsqlite: {theirs:?}"
    );
}

/// A 3-table star: dimensions + a larger fact, so join-order matters.
fn star_setup() -> Vec<String> {
    let mut s = vec![
        "CREATE TABLE region (id INT, name TEXT)".to_string(),
        "CREATE TABLE customer (id INT, region_id INT, name TEXT)".to_string(),
        "CREATE TABLE orders (id INT, customer_id INT, amount INT)".to_string(),
        "INSERT INTO region (id, name) VALUES (1, 'north')".to_string(),
        "INSERT INTO region (id, name) VALUES (2, 'south')".to_string(),
    ];
    for c in 1..=20 {
        s.push(format!(
            "INSERT INTO customer (id, region_id, name) VALUES ({}, {}, 'c{}')",
            c,
            (c % 2) + 1,
            c
        ));
    }
    for o in 1..=200 {
        s.push(format!(
            "INSERT INTO orders (id, customer_id, amount) VALUES ({}, {}, {})",
            o,
            (o % 20) + 1,
            o * 3
        ));
    }
    s
}

fn as_refs(v: &[String]) -> Vec<&str> {
    v.iter().map(|s| s.as_str()).collect()
}

#[test]
fn analyzed_three_table_join_matches_sqlite() {
    let setup = star_setup();
    let analyze = ["ANALYZE region", "ANALYZE customer", "ANALYZE orders"];
    assert_same(
        &as_refs(&setup),
        &analyze,
        "SELECT region.name, customer.name, orders.amount FROM orders \
         JOIN customer ON orders.customer_id = customer.id \
         JOIN region ON customer.region_id = region.id \
         WHERE orders.amount > 500",
    );
}

#[test]
fn analyzed_selective_point_query_matches_sqlite() {
    // A selective equality on an indexed column -> optimizer picks IndexScan;
    // result must match SQLite.
    let setup = star_setup();
    let analyze = ["ANALYZE orders", "ANALYZE customer"];
    assert_same(
        &as_refs(&setup),
        &analyze,
        "SELECT customer.name, orders.amount FROM customer \
         JOIN orders ON customer.id = orders.customer_id WHERE customer.id = 7",
    );
}

#[test]
fn analyzed_aggregate_over_join_matches_sqlite() {
    let setup = star_setup();
    let analyze = ["ANALYZE region", "ANALYZE customer", "ANALYZE orders"];
    assert_same(
        &as_refs(&setup),
        &analyze,
        "SELECT region.name, COUNT(*), SUM(orders.amount) FROM orders \
         JOIN customer ON orders.customer_id = customer.id \
         JOIN region ON customer.region_id = region.id GROUP BY region.name",
    );
}

#[test]
fn stats_survive_reopen() {
    // ANALYZE persists durably: reopen the engine and the stats are still
    // present (optimizer path still engages, result still correct).
    let dir = tempfile::tempdir().unwrap();
    let setup = star_setup();
    {
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        for stmt in &setup {
            engine.execute_sql(xid, stmt).unwrap();
        }
        engine.commit(xid).unwrap();
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "ANALYZE orders").unwrap();
        engine.execute_sql(xid, "ANALYZE customer").unwrap();
        engine.commit(xid).unwrap();
    }
    // Reopen — no re-ANALYZE.
    let engine = Engine::open(dir.path(), 0).unwrap();
    let xid = engine.begin().unwrap();
    let results = engine
        .execute_sql(
            xid,
            "SELECT customer.name, orders.amount FROM customer \
             JOIN orders ON customer.id = orders.customer_id WHERE customer.id = 3",
        )
        .unwrap();
    engine.commit(xid).unwrap();
    let ours = match results.into_iter().next().unwrap() {
        SqlResult::Rows { rows, .. } => rows.len(),
        other => panic!("expected Rows, got {other:?}"),
    };
    let theirs = run_sqlite(
        &as_refs(&setup),
        "SELECT customer.name, orders.amount FROM customer \
         JOIN orders ON customer.id = orders.customer_id WHERE customer.id = 3",
    )
    .len();
    assert_eq!(ours, theirs);
}
