// Item 109 — Step-0 phase attribution for the filtered-SELECT B-tree path.
//
// Mirrors Table 3's workload exactly: t(id INT, k INT, g INT, body TEXT),
// BTREE on k, 100k rows, `SELECT id, body FROM t WHERE k >= 0 AND k < 5000`
// (5% selectivity, compound predicate → per-survivor re-check + two-phase
// decode, same as `sql_crud_select_filtered` in benches/decompose.rs).
//
// Phases:
//   leaf     — B-tree range walk producing candidate RowIds
//              (`search_range_partition`, Q109_LEAF_NANOS)
//   resolve  — parallel heap fetch + MVCC visibility + predicate re-check +
//              decode/project (`parallel_resolve_partitions`, Q109_RESOLVE_NANOS)
//   fetch-only probe — same resolve loop with per_candidate skipped
//              (Q109_FETCH_ONLY): isolates heap fetch + visibility; the
//              decode/project share is (resolve − fetch_only).
//
// Run:  ITEM109_N=100000 cargo test --release --test perf_item109 -- --nocapture

use std::sync::atomic::Ordering as AtomicOrd;
use std::time::Instant;
use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::sql::parallel_scan::{
    item109_reset, Q109_CANDIDATES, Q109_FETCH_ONLY, Q109_LAST_DEGREE, Q109_LEAF_NANOS,
    Q109_PAR_QUERIES, Q109_RESOLVE_NANOS,
};
use unidb::Engine;

const GROUPS: i64 = 10;

fn build(engine: &Engine, rows: i64) {
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
                    Literal::Int(i),
                    Literal::Int(i),
                    Literal::Int(i % GROUPS),
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
    let x = engine.begin().unwrap();
    let _ = engine.execute_sql(x, "ANALYZE t");
    engine.commit(x).unwrap();
}

fn run_queries(engine: &Engine, sql: &str, n: usize) -> f64 {
    let start = Instant::now();
    for _ in 0..n {
        let x = engine.begin().unwrap();
        engine.execute_sql(x, sql).unwrap();
        engine.commit(x).unwrap();
    }
    start.elapsed().as_secs_f64() * 1e6 / n as f64
}

#[test]
fn filtered_select_phase_split() {
    let n: i64 = std::env::var("ITEM109_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let hi = n / 20; // 5% selectivity, matching the bench
    let sql = format!("SELECT id, body FROM t WHERE k >= 0 AND k < {hi}");

    let dir = tempdir().unwrap();
    let engine = Engine::open_with_pool_capacity(dir.path(), 0, 512_000).unwrap();
    engine.set_deferred_sync(true);
    build(&engine, n);

    // Warm-up (page cache + plan cache + worker pool).
    run_queries(&engine, &sql, 3);

    // ── Pass 1: normal path ──────────────────────────────────────────────────
    const RUNS: usize = 20;
    item109_reset();
    let query_us = run_queries(&engine, &sql, RUNS);
    let pq = Q109_PAR_QUERIES.load(AtomicOrd::Relaxed).max(1) as f64;
    let leaf_us = Q109_LEAF_NANOS.load(AtomicOrd::Relaxed) as f64 / 1e3 / pq;
    let resolve_us = Q109_RESOLVE_NANOS.load(AtomicOrd::Relaxed) as f64 / 1e3 / pq;
    let cands = Q109_CANDIDATES.load(AtomicOrd::Relaxed) as f64 / pq;
    let degree = Q109_LAST_DEGREE.load(AtomicOrd::Relaxed);
    let engaged = Q109_PAR_QUERIES.load(AtomicOrd::Relaxed) as usize;

    // ── Pass 2: fetch-only probe (isolates heap fetch + visibility) ─────────
    item109_reset();
    Q109_FETCH_ONLY.store(true, AtomicOrd::Relaxed);
    let _ = run_queries(&engine, &sql, RUNS);
    Q109_FETCH_ONLY.store(false, AtomicOrd::Relaxed);
    let pq2 = Q109_PAR_QUERIES.load(AtomicOrd::Relaxed).max(1) as f64;
    let fetch_us = Q109_RESOLVE_NANOS.load(AtomicOrd::Relaxed) as f64 / 1e3 / pq2;

    let decode_us = (resolve_us - fetch_us).max(0.0);
    let other_us = (query_us - leaf_us - resolve_us).max(0.0);

    eprintln!();
    eprintln!("═══════════════════════════════════════════════════════════════════════");
    eprintln!(" Item 109 Step-0 — filtered SELECT phase split ({n} rows, 5% sel)");
    eprintln!("═══════════════════════════════════════════════════════════════════════");
    eprintln!(
        " Parallel path engaged      : {engaged}/{RUNS} queries · degree {degree} · {cands:.0} candidates/q"
    );
    eprintln!(" Whole query (avg)          : {query_us:>8.1} µs");
    eprintln!(
        " ── B-tree leaf walk        : {leaf_us:>8.1} µs  ({:>4.1}%)",
        leaf_us / query_us * 100.0
    );
    eprintln!(
        " ── candidate resolution    : {resolve_us:>8.1} µs  ({:>4.1}%)",
        resolve_us / query_us * 100.0
    );
    eprintln!("     · heap fetch+visibility: {fetch_us:>8.1} µs  (fetch-only probe)");
    eprintln!("     · decode+pred+project  : {decode_us:>8.1} µs  (by subtraction)");
    eprintln!(" ── everything else         : {other_us:>8.1} µs  (parse/plan/txn/assembly)");
    eprintln!("═══════════════════════════════════════════════════════════════════════");
    eprintln!();

    // Step-0 is a measurement, not a gate — but the parallel path must have
    // actually engaged for the numbers to mean anything.
    assert!(
        engaged == RUNS,
        "parallel range path engaged on {engaged}/{RUNS} queries — probe invalid \
         (workers disabled or below candidate threshold?)"
    );
}
