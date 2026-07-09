//! P4.e — EXPLAIN / EXPLAIN ANALYZE. Verifies the rendered plan reflects the
//! chosen operators (including the optimizer's index-vs-scan decision) and that
//! EXPLAIN ANALYZE populates actual rows + execution time.

use unidb::sql::logical::Literal;
use unidb::{Engine, SqlResult};

fn explain_lines(engine: &mut Engine, sql: &str) -> Vec<String> {
    let xid = engine.begin().unwrap();
    let results = engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
    match results.into_iter().next().unwrap() {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match &r[0] {
                Literal::Text(s) => s.clone(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn setup(engine: &mut Engine, analyze: bool) {
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE customer (id INT, name TEXT)")
        .unwrap();
    engine
        .execute_sql(
            xid,
            "CREATE TABLE orders (id INT, customer_id INT, amount INT)",
        )
        .unwrap();
    engine
        .execute_sql(xid, "CREATE INDEX ci ON customer USING BTREE (id)")
        .unwrap();
    for c in 1..=50 {
        engine
            .execute_sql(
                xid,
                &format!("INSERT INTO customer (id, name) VALUES ({c}, 'c{c}')"),
            )
            .unwrap();
    }
    for o in 1..=200 {
        engine
            .execute_sql(
                xid,
                &format!(
                    "INSERT INTO orders (id, customer_id, amount) VALUES ({}, {}, {})",
                    o,
                    (o % 50) + 1,
                    o
                ),
            )
            .unwrap();
    }
    engine.commit(xid).unwrap();
    if analyze {
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "ANALYZE customer").unwrap();
        engine.execute_sql(xid, "ANALYZE orders").unwrap();
        engine.commit(xid).unwrap();
    }
}

#[test]
fn explain_shows_join_plan_tree() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    setup(&mut engine, true);
    let lines = explain_lines(
        &mut engine,
        "EXPLAIN SELECT customer.name, orders.amount FROM customer \
         JOIN orders ON customer.id = orders.customer_id",
    );
    let text = lines.join("\n");
    assert!(text.contains("Projection"), "plan:\n{text}");
    assert!(
        text.contains("HashJoin") || text.contains("IndexNestedLoopJoin"),
        "plan should contain a join operator:\n{text}"
    );
    // Estimated row counts are annotated.
    assert!(text.contains("est_rows="), "plan:\n{text}");
}

#[test]
fn explain_analyze_populates_actuals() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    setup(&mut engine, true);
    let lines = explain_lines(
        &mut engine,
        "EXPLAIN ANALYZE SELECT customer.name, orders.amount FROM customer \
         JOIN orders ON customer.id = orders.customer_id WHERE orders.amount > 150",
    );
    let text = lines.join("\n");
    assert!(text.contains("actual_rows="), "plan:\n{text}");
    assert!(text.contains("execution_time_ms="), "plan:\n{text}");
}

#[test]
fn explain_reflects_index_vs_scan_choice() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    setup(&mut engine, true);

    // Selective equality on the indexed `customer.id` (1/50) -> IndexScan.
    let selective = explain_lines(
        &mut engine,
        "EXPLAIN SELECT name FROM customer WHERE id = 7",
    )
    .join("\n");
    assert!(
        selective.contains("IndexScan customer on id"),
        "selective query should index-scan:\n{selective}"
    );

    // Range covering nearly all rows -> full Scan (not an index scan).
    let broad = explain_lines(
        &mut engine,
        "EXPLAIN SELECT name FROM customer WHERE id > 0",
    )
    .join("\n");
    assert!(
        !broad.contains("IndexScan"),
        "unselective query should full-scan:\n{broad}"
    );
    assert!(broad.contains("Scan customer"), "plan:\n{broad}");
}

#[test]
fn explain_without_analyze_does_not_require_stats() {
    // EXPLAIN (no ANALYZE stats gathered) still renders a plan.
    let dir = tempfile::tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    setup(&mut engine, false);
    let lines = explain_lines(
        &mut engine,
        "EXPLAIN SELECT customer.name, orders.amount FROM customer \
         JOIN orders ON customer.id = orders.customer_id",
    );
    assert!(!lines.is_empty());
    assert!(lines.join("\n").contains("est_rows="));
}
