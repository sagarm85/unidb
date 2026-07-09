// P4 (query power) benchmark: a small TPC-H-subset showing joins, aggregation,
// grouping, and the cost-based optimizer working end-to-end, with honest
// single-node latencies. This is NOT the CLAUDE.md §6 "replaced stack" headline
// (that's a separate cross-domain effort) — it measures the query engine this
// phase built, on its own.
//
// Schema (subset): customer, orders (FK customer), lineitem (FK orders). After
// ANALYZE, the optimizer chooses join order + index-vs-scan; we time three
// representative queries plus the index-vs-scan crossover.
//
// Dataset is deliberately modest: the whole catalog (every TableDef's page list
// + all ANALYZE stats) is one ~8 KiB page blob, so a table's page list must fit
// there — a multi-page catalog is tracked tech debt. Numbers are
// machine-dependent; the point is that joins/aggregates run at interactive
// latencies and the optimizer picks the index plan for a selective predicate.
// Run with: cargo bench --bench tpch

use std::time::Instant;

use tempfile::tempdir;
use unidb::{Engine, SqlResult};

const CUSTOMERS: usize = 200;
const ORDERS: usize = 2_000;
const LINEITEMS: usize = 6_000;

fn build(engine: &mut Engine) {
    let xid = engine.begin().unwrap();
    for stmt in [
        "CREATE TABLE customer (id INT, nation INT)",
        "CREATE TABLE orders (id INT, customer_id INT, total INT)",
        "CREATE TABLE lineitem (id INT, order_id INT, price INT)",
        "CREATE INDEX ci ON customer USING BTREE (id)",
        "CREATE INDEX oci ON orders USING BTREE (customer_id)",
    ] {
        engine.execute_sql(xid, stmt).unwrap();
    }
    engine.commit(xid).unwrap();

    let load = |sql: String| {
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, &sql).unwrap();
        engine.commit(xid).unwrap();
    };
    for c in 0..CUSTOMERS {
        load(format!(
            "INSERT INTO customer (id, nation) VALUES ({c}, {})",
            c % 25
        ));
    }
    for o in 0..ORDERS {
        load(format!(
            "INSERT INTO orders (id, customer_id, total) VALUES ({o}, {}, {})",
            o % CUSTOMERS,
            (o % 1000) as i64
        ));
    }
    for l in 0..LINEITEMS {
        load(format!(
            "INSERT INTO lineitem (id, order_id, price) VALUES ({l}, {}, {})",
            l % ORDERS,
            (l % 200) + 1
        ));
    }

    let xid = engine.begin().unwrap();
    for t in ["customer", "orders", "lineitem"] {
        engine.execute_sql(xid, &format!("ANALYZE {t}")).unwrap();
    }
    engine.commit(xid).unwrap();
}

fn time_query(engine: &mut Engine, sql: &str, iters: usize) -> (f64, f64, usize) {
    let mut latencies = Vec::with_capacity(iters);
    let mut rows_out = 0;
    for _ in 0..iters {
        let xid = engine.begin().unwrap();
        let start = Instant::now();
        let res = engine.execute_sql(xid, sql).unwrap();
        latencies.push(start.elapsed().as_secs_f64() * 1000.0);
        engine.commit(xid).unwrap();
        if let Some(SqlResult::Rows(r)) = res.into_iter().next() {
            rows_out = r.len();
        }
    }
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = latencies[latencies.len() / 2];
    let p99 = latencies[(latencies.len() * 99 / 100).min(latencies.len() - 1)];
    (p50, p99, rows_out)
}

fn main() {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    println!(
        "Building TPC-H subset: {CUSTOMERS} customers, {ORDERS} orders, {LINEITEMS} lineitems..."
    );
    let t0 = Instant::now();
    build(&mut engine);
    println!("  build + ANALYZE: {:.1} s\n", t0.elapsed().as_secs_f64());

    let queries: &[(&str, &str)] = &[
        (
            "Q1 join+filter (orders join customer, selective)",
            "SELECT customer.nation, orders.total FROM orders \
             JOIN customer ON orders.customer_id = customer.id WHERE customer.id = 42",
        ),
        (
            "Q2 group-by aggregate (orders by customer)",
            "SELECT customer_id, COUNT(*), SUM(total) FROM orders GROUP BY customer_id",
        ),
        (
            "Q3 3-way join + group-by + aggregate",
            "SELECT customer.nation, COUNT(*), SUM(lineitem.price) FROM lineitem \
             JOIN orders ON lineitem.order_id = orders.id \
             JOIN customer ON orders.customer_id = customer.id \
             GROUP BY customer.nation",
        ),
        (
            "Q4 order-by + limit (top orders)",
            "SELECT id, total FROM orders ORDER BY total DESC LIMIT 10",
        ),
    ];

    println!(
        "{:<50} {:>10} {:>10} {:>8}",
        "query", "p50 (ms)", "p99 (ms)", "rows"
    );
    println!("{}", "-".repeat(82));
    for (name, sql) in queries {
        let (p50, p99, rows) = time_query(&mut engine, sql, 30);
        println!("{name:<50} {p50:>10.3} {p99:>10.3} {rows:>8}");
    }

    // Index-vs-scan crossover: EXPLAIN proves the plan the optimizer chose.
    let plan = |engine: &mut Engine, sql: &str| -> String {
        let xid = engine.begin().unwrap();
        let res = engine.execute_sql(xid, sql).unwrap();
        engine.commit(xid).unwrap();
        match res.into_iter().next().unwrap() {
            SqlResult::Rows(rows) => rows
                .iter()
                .map(|r| match &r[0] {
                    unidb::sql::logical::Literal::Text(s) => s.clone(),
                    o => format!("{o:?}"),
                })
                .collect::<Vec<_>>()
                .join(" | "),
            _ => String::new(),
        }
    };
    println!("\nOptimizer plan choice (EXPLAIN):");
    println!(
        "  selective (customer.id = 42): {}",
        plan(
            &mut engine,
            "EXPLAIN SELECT nation FROM customer WHERE id = 42"
        )
    );
    println!(
        "  broad     (customer.id > 0) : {}",
        plan(
            &mut engine,
            "EXPLAIN SELECT nation FROM customer WHERE id > 0"
        )
    );
}
