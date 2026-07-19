// Fresh-mind profiling harness for bulk UPDATE / DELETE selected (Table 3 mirror).
// NOT part of the repo's bench suite — temporary analysis artifact.
//
// Usage:
//   cargo run --release --example profile_bulk_dml            # timed phases, 3 iterations
//   PROFILE_ITERS=10 cargo run --release --example profile_bulk_dml
//
// Mirrors decompose.rs Table 3: table t(id,k,g,body), btree on k, 100k rows,
// deferred_sync(true), UPDATE SET body='updated' WHERE k<half (HOT-eligible),
// DELETE WHERE k>=half. Each iteration uses a fresh engine dir.

use std::time::Instant;

use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::Engine;

const ROWS: u64 = 100_000;
const CRUD_GROUPS: i64 = 100;

fn build(engine: &Engine, rows: u64) {
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
        .unwrap();
    engine
        .execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
        .unwrap();
    engine.commit(x).unwrap();
    let ins = engine
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();
    let mut x = engine.begin().unwrap();
    for i in 0..rows {
        engine
            .execute_prepared(
                x,
                &ins,
                &[
                    Literal::Int(i as i64),
                    Literal::Int(i as i64),
                    Literal::Int((i as i64) % CRUD_GROUPS),
                    Literal::Text(format!("b{i}")),
                ],
            )
            .unwrap();
        if (i + 1) % 5_000 == 0 {
            engine.commit(x).unwrap();
            x = engine.begin().unwrap();
        }
    }
    engine.commit(x).unwrap();
    let ax = engine.begin().unwrap();
    let _ = engine.execute_sql(ax, "ANALYZE t");
    engine.commit(ax).unwrap();
}

fn main() {
    let iters: u64 = std::env::var("PROFILE_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let half = (ROWS / 2) as i64;

    let mut upd_secs = 0.0;
    let mut del_secs = 0.0;
    let mut build_secs = 0.0;

    for it in 0..iters {
        let dir = tempdir().unwrap();
        let engine = Engine::open_with_pool_capacity(dir.path(), 0, 200_000).unwrap();
        engine.set_deferred_sync(true);

        let t0 = Instant::now();
        build(&engine, ROWS);
        build_secs += t0.elapsed().as_secs_f64();

        // UPDATE bulk (HOT-eligible: body not indexed), 50k of 100k rows.
        let x = engine.begin().unwrap();
        let t1 = Instant::now();
        let _ = engine
            .execute_sql(x, &format!("UPDATE t SET body = 'updated' WHERE k < {half}"))
            .unwrap();
        engine.commit(x).unwrap();
        let u = t1.elapsed().as_secs_f64();
        upd_secs += u;

        // DELETE selected: the other 50k rows (predicate path, 50% of table).
        let x = engine.begin().unwrap();
        let t2 = Instant::now();
        let _ = engine
            .execute_sql(x, &format!("DELETE FROM t WHERE k >= {half}"))
            .unwrap();
        engine.commit(x).unwrap();
        let d = t2.elapsed().as_secs_f64();
        del_secs += d;

        println!(
            "iter {it}: update 50k in {u:.3}s ({:.0} rec/s) | delete 50k in {d:.3}s ({:.0} rec/s)",
            50_000.0 / u,
            50_000.0 / d
        );
    }

    println!("---");
    println!(
        "avg build:  {:.3}s ({:.0} rec/s)",
        build_secs / iters as f64,
        ROWS as f64 * iters as f64 / build_secs
    );
    println!(
        "avg UPDATE: {:.3}s ({:.0} rec/s)",
        upd_secs / iters as f64,
        50_000.0 * iters as f64 / upd_secs
    );
    println!(
        "avg DELETE: {:.3}s ({:.0} rec/s)",
        del_secs / iters as f64,
        50_000.0 * iters as f64 / del_secs
    );
}
