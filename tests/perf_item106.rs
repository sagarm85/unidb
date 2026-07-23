// Item 106 Step-0 — recall-vs-ef curve for the CURRENT graph at 10k×dim128.
//
// The curve decides the lever ordering for the ≤400 µs tier
// (docs/backlog/106_vector_pgvector_class_tier.md):
//   steep curve (recall collapses below ef≈200)  → graph quality (L1) first
//   flat curve  (0.90 held down to ef≈80)        → ef-retune + SQ8 (L2) first
//
// Measures, per ef ∈ {40, 60, 80, 120, 160, 200, 300}:
//   recall@10 vs exact brute force (after the executor's exact re-rank —
//   the user-visible recall), and warm NEAR latency (avg of 14 queries,
//   caches hot, same methodology as perf_item92).
//
// Run:  ITEM106_N=10000 cargo test --release --test perf_item106 -- --nocapture

use std::collections::HashSet;
use std::time::Instant;
use tempfile::tempdir;
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

const DIM: usize = 128;
const K: usize = 10;
const N_QUERIES: usize = 20;

/// Deterministic vector from a seed (same LCG as perf_item92 / decompose).
fn rand_vec(seed: u64) -> Vec<f32> {
    let mut s = seed.wrapping_add(1);
    (0..DIM)
        .map(|_| {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (s >> 32) as f32 / u32::MAX as f32 * 2.0 - 1.0
        })
        .collect()
}

fn near_sql(q: &[f32], k: usize) -> String {
    let coords: Vec<String> = q.iter().map(|f| format!("{f:.8}")).collect();
    format!(
        "SELECT id FROM t WHERE NEAR(embedding, [{}], {k})",
        coords.join(", ")
    )
}

fn near_ids(engine: &Engine, sql: &str) -> Vec<i64> {
    let x = engine.begin().unwrap();
    let res = engine.execute_sql(x, sql).unwrap();
    engine.commit(x).unwrap();
    match res.into_iter().next().unwrap() {
        ExecResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r[0] {
                Literal::Int(i) => *i,
                other => panic!("id not int: {other:?}"),
            })
            .collect(),
        other => panic!("expected rows: {other:?}"),
    }
}

fn euclid(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

#[test]
fn recall_vs_ef_curve() {
    let n: usize = std::env::var("ITEM106_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    let dir = tempdir().unwrap();
    let engine = Engine::open_with_pool_capacity(dir.path(), 0, 512_000).unwrap();
    engine.set_deferred_sync(true);

    // Build corpus + HNSW index (current build path).
    let x = engine.begin().unwrap();
    engine
        .execute_sql(
            x,
            &format!("CREATE TABLE t (id INT, embedding VECTOR({DIM}))"),
        )
        .unwrap();
    engine.commit(x).unwrap();
    let ins = engine
        .prepare("INSERT INTO t (id, embedding) VALUES ($1, $2)")
        .unwrap();
    let mut x = engine.begin().unwrap();
    for i in 0..n {
        engine
            .execute_prepared(
                x,
                &ins,
                &[Literal::Int(i as i64), Literal::Vector(rand_vec(i as u64))],
            )
            .unwrap();
        if (i + 1) % 5_000 == 0 {
            engine.commit(x).unwrap();
            x = engine.begin().unwrap();
        }
    }
    engine.commit(x).unwrap();
    let x = engine.begin().unwrap();
    let t0 = Instant::now();
    engine
        .execute_sql(x, "CREATE INDEX iv ON t USING HNSW (embedding)")
        .unwrap();
    engine.commit(x).unwrap();
    eprintln!(
        "[item106] CREATE INDEX on {n} rows: {} ms",
        t0.elapsed().as_millis()
    );

    // Query set + brute-force ground truth (computed once).
    let queries: Vec<Vec<f32>> = (0..N_QUERIES)
        .map(|i| rand_vec(900_000 + i as u64))
        .collect();
    let corpus: Vec<Vec<f32>> = (0..n).map(|i| rand_vec(i as u64)).collect();
    let truth: Vec<HashSet<i64>> = queries
        .iter()
        .map(|q| {
            let mut scored: Vec<(f32, i64)> = corpus
                .iter()
                .enumerate()
                .map(|(i, v)| (euclid(q, v), i as i64))
                .collect();
            scored.sort_by(|a, b| a.0.total_cmp(&b.0));
            scored.iter().take(K).map(|(_, i)| *i).collect()
        })
        .collect();
    let sqls: Vec<String> = queries.iter().map(|q| near_sql(q, K)).collect();

    eprintln!();
    eprintln!("═══════════════════════════════════════════════════════════════");
    eprintln!(" Item 106 Step-0 — recall@{K} vs ef_search ({n}×dim{DIM}, current graph)");
    eprintln!("═══════════════════════════════════════════════════════════════");
    eprintln!(
        " {:>4} │ {:>9} │ {:>12} │ note",
        "ef", "recall@10", "warm µs/q"
    );
    eprintln!(" ─────┼───────────┼──────────────┼──────────────────");

    for &ef in &[40usize, 60, 80, 120, 160, 200, 300] {
        unidb::hnsw_index::set_ef_search(ef);
        // Warm-up (cache population happens once globally; keep 3 per ef for
        // plan/branch warmth symmetry across points).
        for sql in sqls.iter().take(3) {
            let _ = near_ids(&engine, sql);
        }
        // Recall over all queries + timed pass over the last 14.
        let mut hits = 0usize;
        let t = Instant::now();
        let mut timed = 0usize;
        let mut timed_ns = 0u128;
        for (qi, sql) in sqls.iter().enumerate() {
            let t1 = Instant::now();
            let ids = near_ids(&engine, sql);
            if qi >= N_QUERIES - 14 {
                timed_ns += t1.elapsed().as_nanos();
                timed += 1;
            }
            hits += ids.iter().filter(|i| truth[qi].contains(i)).count();
        }
        let _ = t;
        let recall = hits as f64 / (N_QUERIES * K) as f64;
        let warm_us = timed_ns as f64 / 1e3 / timed as f64;
        let note = if recall >= 0.90 {
            "meets gate"
        } else {
            "BELOW 0.90"
        };
        eprintln!(" {ef:>4} │ {recall:>9.3} │ {warm_us:>12.1} │ {note}");
    }
    eprintln!("═══════════════════════════════════════════════════════════════");
    unidb::hnsw_index::set_ef_search(unidb::hnsw_index::HNSW_EF_SEARCH);
}
