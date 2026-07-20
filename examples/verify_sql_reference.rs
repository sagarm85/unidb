// Verification harness for docs/sql/sql_reference.md — runs one statement per
// documented command family through the correct entry point (execute_sql /
// execute_sql_as / execute_cypher) on a fresh engine and prints OK/ERR. Keep it
// in sync with the reference so the doc only ever documents syntax the parser
// accepts. Run: `cargo run --release --example verify_sql_reference`.

use tempfile::tempdir;
use unidb::Engine;

fn main() {
    let dir = tempdir().unwrap();
    let e = Engine::open(dir.path(), 0).unwrap();

    let sql: &[&str] = &[
        // DDL
        "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT UNIQUE, active BOOL)",
        "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT REFERENCES customers(id), amount INT, status TEXT)",
        "CREATE TABLE docs (id INT, body TEXT, embedding VECTOR(4))",
        "CREATE INDEX idx_status ON orders USING BTREE (status)",
        "CREATE INDEX idx_body ON docs USING FULLTEXT (body)",
        "CREATE INDEX idx_emb ON docs USING HNSW (embedding)",
        "ALTER TABLE customers ADD COLUMN tier INT DEFAULT 1",
        "ANALYZE customers",
        // DML
        "INSERT INTO customers (id, name, active) VALUES (1, 'alice', true)",
        "INSERT INTO customers (id, name, active) VALUES (2, 'bob', false)",
        "INSERT INTO orders (id, customer_id, amount, status) VALUES (10, 1, 500, 'open')",
        "INSERT INTO docs (id, body, embedding) VALUES (1, 'invoice overdue', [0.1, 0.2, 0.3, 0.4])",
        "UPDATE orders SET status = 'shipped' WHERE id = 10",
        "DELETE FROM orders WHERE id = 10 RETURNING id, status",
        // Queries
        "SELECT id, name FROM customers WHERE active = true",
        "SELECT COUNT(*) FROM customers",
        "SELECT status, COUNT(*) FROM orders GROUP BY status",
        "SELECT name FROM customers ORDER BY id",
        "SELECT * FROM orders JOIN customers USING (id)",
        "SELECT id FROM docs WHERE MATCH(body, 'invoice')",
        "SELECT * FROM docs WHERE NEAR(embedding, [0.0, 0.0, 0.0, 0.0], 3)",
        "EXPLAIN SELECT name FROM customers WHERE id = 1",
    ];

    // Auth / RLS DDL goes through execute_sql_as (None = embedded superuser), NOT execute_sql.
    let auth: &[&str] = &[
        "CREATE USER carol",
        "CREATE ROLE analyst",
        "GRANT SELECT, INSERT ON customers TO carol",
        "CREATE POLICY sel_own ON docs FOR SELECT USING (id > 0)",
        "CREATE POLICY upd_chk ON orders FOR UPDATE USING (amount >= 0) WITH CHECK (amount >= 0)",
        "REVOKE INSERT ON customers FROM carol",
    ];

    let mut ok = 0;
    let mut err = 0;
    for s in sql {
        let x = e.begin().unwrap();
        match e.execute_sql(x, s) {
            Ok(_) => {
                e.commit(x).unwrap();
                ok += 1;
                println!("OK    {s}");
            }
            Err(err_) => {
                let _ = e.abort(x);
                err += 1;
                println!("ERR   {s}\n        -> {err_}");
            }
        }
    }

    for s in auth {
        let x = e.begin().unwrap();
        match e.execute_sql_as(None, x, s) {
            Ok(_) => {
                e.commit(x).unwrap();
                ok += 1;
                println!("OK    [auth] {s}");
            }
            Err(er) => {
                let _ = e.abort(x);
                err += 1;
                println!("ERR   [auth] {s}\n        -> {er}");
            }
        }
    }

    // Cypher (separate entry point, read-only). Seed one edge via the embedded API.
    let x = e.begin().unwrap();
    let _ = e.create_edge(x, 1, 2, "KNOWS", "{}");
    e.commit(x).unwrap();
    let x = e.begin().unwrap();
    match e.execute_cypher(x, "MATCH (a)-[:KNOWS]->(b) WHERE a = 1 RETURN b") {
        Ok(_) => {
            e.commit(x).unwrap();
            ok += 1;
            println!("OK    [cypher] MATCH (a)-[:KNOWS]->(b) WHERE a = 1 RETURN b");
        }
        Err(er) => {
            let _ = e.abort(x);
            err += 1;
            println!("ERR   [cypher] MATCH ... -> {er}");
        }
    }

    println!("\n=== {ok} OK, {err} ERR ===");
}
