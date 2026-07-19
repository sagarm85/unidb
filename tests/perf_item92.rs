// Item 92 — vector query next tier: Step-0 profiling + lever validation.
//
// ## Purpose
//
// This test provides the mandatory Step-0 profile (§0.6) for
// docs/backlog/92_vector_query_next_tier.md by counting per-hop cost sources
// during warm NEAR queries.
//
// Counters used (defined in hnsw_index.rs):
//   Q_L0_CACHE_HITS  — get_l0_nbrs returned from in-memory L0 cache (no disk)
//   Q_VEC_CACHE_HITS — fetch_vector_cached_with_vec returned from vec cache (no disk)
//   Q_DISK_FETCHES   — load_node_at called (DiskBTree lookup + page fetch)
//   Q_DISTANCE_CALLS — hnsw_distance called (each = dim f32 multiply-add ops)
//
// ## Warm vs cold terminology
//
// - Cold: first NEAR query; both L0 and vec caches empty → many disk fetches.
// - Warm: subsequent queries after caches are populated → zero disk fetches expected.
//
// ## Corpus size
//
// CI uses N=2000×dim128 (fast, ~5 s build).  For the full 10k profile run:
//   ITEM92_N=10000 cargo test --release --test perf_item92 -- --nocapture
//
// ## Tests
//
// 1. `hnsw_step0_profile`   — prints counter profile table (always passes).
// 2. `hnsw_recall_gate_2k`  — recall@10 ≥ 0.90 at 2k×dim128 (regression gate).

use std::collections::HashSet;
use std::sync::atomic::Ordering as AtomicOrd;
use std::time::Instant;
use tempfile::tempdir;
use unidb::hnsw_index::{Q_DISK_FETCHES, Q_DISTANCE_CALLS, Q_L0_CACHE_HITS, Q_VEC_CACHE_HITS};
use unidb::sql::logical::Literal;
use unidb::Engine;

const DIM: usize = 128;
const K: usize = 10;
const EF: usize = 200; // HNSW_EF_SEARCH

/// Deterministic 128-dim vector from a seed (same LCG as decompose bench).
fn rand_vec_128(seed: u64) -> Vec<f32> {
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

fn near_sql(query_vec: &[f32], k: usize) -> String {
    let coords: Vec<String> = query_vec.iter().map(|f| format!("{f:.8}")).collect();
    format!(
        "SELECT id FROM t WHERE NEAR(embedding, [{}], {k})",
        coords.join(", ")
    )
}

fn euclidean_dist_sq(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = (*x as f64) - (*y as f64);
            d * d
        })
        .sum()
}

/// Build a unidb engine with N rows × dim128 HNSW index.
/// Returns (engine, tempdir_guard, corpus_vecs, query_sqls).
fn build_engine(n: usize) -> (Engine, tempfile::TempDir, Vec<Vec<f32>>, Vec<String>) {
    let dir = tempdir().unwrap();
    // Use a large enough pool so the whole index fits in memory for warm queries.
    let engine = Engine::open_with_pool_capacity(dir.path(), 0, 512_000).unwrap();
    engine.set_deferred_sync(true);

    let setup_xid = engine.begin().unwrap();
    engine
        .execute_sql(
            setup_xid,
            "CREATE TABLE t (id INT PRIMARY KEY, embedding VECTOR(128))",
        )
        .unwrap();
    engine.commit(setup_xid).unwrap();

    // Insert in 500-row chunks to keep WAL size manageable.
    let corpus_vecs: Vec<Vec<f32>> = (0..n).map(|i| rand_vec_128(i as u64)).collect();
    for chunk in (0..n).collect::<Vec<_>>().chunks(500) {
        let xid = engine.begin().unwrap();
        for &i in chunk {
            let v = &corpus_vecs[i];
            let coords: Vec<String> = v.iter().map(|f| format!("{f:.8}")).collect();
            engine
                .execute_sql(
                    xid,
                    &format!(
                        "INSERT INTO t (id, embedding) VALUES ({i}, [{}])",
                        coords.join(", ")
                    ),
                )
                .unwrap();
        }
        engine.commit(xid).unwrap();
    }

    // Build HNSW index in a single commit (uses fast two-pass bulk build).
    let t_idx = Instant::now();
    let ixid = engine.begin().unwrap();
    engine
        .execute_sql(ixid, "CREATE INDEX iv ON t USING HNSW (embedding)")
        .unwrap();
    engine.commit(ixid).unwrap();
    let idx_ms = t_idx.elapsed().as_secs_f64() * 1e3;
    eprintln!("[item92] CREATE INDEX on {n} rows: {idx_ms:.0} ms");

    let n_queries = 20usize;
    let query_vecs: Vec<Vec<f32>> = (0..n_queries)
        .map(|i| rand_vec_128((n as u64) + 100_000 + i as u64))
        .collect();
    let query_sqls: Vec<String> = query_vecs.iter().map(|q| near_sql(q, K)).collect();

    (engine, dir, corpus_vecs, query_sqls)
}

/// Step-0 profile: count warm NEAR counters and print the table.
/// Run: cargo test --release --test perf_item92 hnsw_step0_profile -- --nocapture
#[test]
fn hnsw_step0_profile() {
    let n: usize = std::env::var("ITEM92_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000);

    let (engine, _dir, corpus_vecs, query_sqls) = build_engine(n);
    let n_queries = query_sqls.len();

    // ── Cold query: first NEAR after index creation (caches empty) ───────────
    unidb::hnsw_index::reset_query_counters();
    let t_cold = Instant::now();
    {
        let w = engine.begin().unwrap();
        engine.execute_sql(w, &query_sqls[0]).unwrap();
        engine.commit(w).unwrap();
    }
    let cold_us = t_cold.elapsed().as_secs_f64() * 1e6;
    let cold_l0 = Q_L0_CACHE_HITS.load(AtomicOrd::Relaxed);
    let cold_vec = Q_VEC_CACHE_HITS.load(AtomicOrd::Relaxed);
    let cold_disk = Q_DISK_FETCHES.load(AtomicOrd::Relaxed);
    let cold_dist = Q_DISTANCE_CALLS.load(AtomicOrd::Relaxed);

    // ── Warm-up: 5 queries to populate caches ────────────────────────────────
    for i in 1..6.min(n_queries) {
        let w = engine.begin().unwrap();
        engine.execute_sql(w, &query_sqls[i]).unwrap();
        engine.commit(w).unwrap();
    }

    // ── Warm queries: measure last N_QUERIES - 6 ─────────────────────────────
    let warm_start_idx = 6.min(n_queries);
    let warm_count = (n_queries - warm_start_idx).max(1);
    unidb::hnsw_index::reset_query_counters();
    let t_warm = Instant::now();
    for i in warm_start_idx..n_queries {
        let w = engine.begin().unwrap();
        engine.execute_sql(w, &query_sqls[i]).unwrap();
        engine.commit(w).unwrap();
    }
    let total_warm_us = t_warm.elapsed().as_secs_f64() * 1e6;
    let warm_us = total_warm_us / warm_count as f64;

    let warm_l0 = Q_L0_CACHE_HITS.load(AtomicOrd::Relaxed);
    let warm_vec = Q_VEC_CACHE_HITS.load(AtomicOrd::Relaxed);
    let warm_disk = Q_DISK_FETCHES.load(AtomicOrd::Relaxed);
    let warm_dist = Q_DISTANCE_CALLS.load(AtomicOrd::Relaxed);

    // ── Also measure txn overhead (begin + commit with no SQL) ───────────────
    let t_noop = Instant::now();
    let noop_iters = 20usize;
    for _ in 0..noop_iters {
        let w = engine.begin().unwrap();
        engine.commit(w).unwrap();
    }
    let txn_us = t_noop.elapsed().as_secs_f64() * 1e6 / noop_iters as f64;

    let pure_query_us = (warm_us - txn_us.min(warm_us)).max(0.0);

    // ── Print table ──────────────────────────────────────────────────────────
    eprintln!();
    eprintln!("═══════════════════════════════════════════════════════════════════════");
    eprintln!(" Item 92 Step-0 Profile — corpus {n}×dim{DIM}, k={K}, ef={EF}");
    eprintln!("═══════════════════════════════════════════════════════════════════════");
    eprintln!(" Latency:");
    eprintln!(
        "   Cold (1st query)       : {:>9.1} µs  (caches empty, all disk fetches)",
        cold_us
    );
    eprintln!(
        "   Warm (avg last {warm_count} q)  : {:>9.1} µs  (caches hot)",
        warm_us
    );
    eprintln!("   Txn overhead (begin+commit no-op): {:>6.1} µs", txn_us);
    eprintln!(
        "   Pure query time (warm - txn)     : {:>6.1} µs",
        pure_query_us
    );
    eprintln!(
        "   Target                : {:>9} µs  (pgvector-class ≤700 µs warm)",
        "≤700"
    );
    eprintln!("───────────────────────────────────────────────────────────────────────");
    eprintln!(" Counter breakdown:");
    eprintln!(" {:<26}  {:>9}  {:>9}  Notes", "metric", "cold", "warm/q");
    eprintln!("───────────────────────────────────────────────────────────────────────");
    let cq = 1u64; // one cold query
    let wq = warm_count as u64;
    eprintln!(
        " {:<26}  {:>9}  {:>9.1}  get_l0_nbrs from cache",
        "L0 cache hits",
        cold_l0 / cq,
        warm_l0 as f64 / wq as f64
    );
    eprintln!(
        " {:<26}  {:>9}  {:>9.1}  fetch_vector from cache",
        "Vec cache hits",
        cold_vec / cq,
        warm_vec as f64 / wq as f64
    );
    eprintln!(
        " {:<26}  {:>9}  {:>9.1}  DiskBTree + page load",
        "Disk fetches",
        cold_disk / cq,
        warm_disk as f64 / wq as f64
    );
    eprintln!(
        " {:<26}  {:>9}  {:>9.1}  hnsw_distance() = {DIM} f32 mul+add",
        "Distance calls",
        cold_dist / cq,
        warm_dist as f64 / wq as f64
    );
    eprintln!("───────────────────────────────────────────────────────────────────────");

    // Per-query analysis.
    let warm_l0_per_q = warm_l0 as f64 / wq as f64;
    let warm_vec_per_q = warm_vec as f64 / wq as f64;
    let warm_disk_per_q = warm_disk as f64 / wq as f64;
    let warm_dist_per_q = warm_dist as f64 / wq as f64;

    // Estimated costs (rough but evidence-based):
    // - Vec clone of 32 RowIds = 192 bytes ≈ 0.05 µs per memcpy
    // - Vec clone of 128 f32 = 512 bytes ≈ 0.15 µs per memcpy
    // - scalar distance dim=128 ≈ 0.08 µs (128 mul+add)
    // - hash lookup in HashMap ≈ 0.05 µs per call
    let est_l0_clone_us = warm_l0_per_q * 0.05;
    let est_vec_clone_us = warm_vec_per_q * 0.15;
    let est_disk_us = warm_disk_per_q * 50.0; // disk I/O cold
    let est_dist_us = warm_dist_per_q * 0.08;
    let est_total_attributed = est_l0_clone_us + est_vec_clone_us + est_disk_us + est_dist_us;
    let est_overhead = (pure_query_us - est_total_attributed).max(0.0);

    eprintln!(" Estimated hot-path cost attribution (per warm query):");
    eprintln!(
        "   L0 nbr Vec clones     : {:>6.1} µs  ({warm_l0_per_q:.0} clones × 192B × ~0.05µs)",
        est_l0_clone_us
    );
    eprintln!(
        "   Vec Vec clones        : {:>6.1} µs  ({warm_vec_per_q:.0} clones × 512B × ~0.15µs)",
        est_vec_clone_us
    );
    eprintln!(
        "   Distance computation  : {:>6.1} µs  ({warm_dist_per_q:.0} calls × dim128 scalar)",
        est_dist_us
    );
    eprintln!(
        "   Remaining disk I/O    : {:>6.1} µs  ({warm_disk_per_q:.0} fetches × ~50µs each)",
        est_disk_us
    );
    eprintln!(
        "   Unattributed overhead : {:>6.1} µs  (txn, hash map overhead, heap re-rank, etc.)",
        est_overhead
    );
    eprintln!();

    // Lever recommendations based on profile.
    eprintln!(" Lever recommendations:");
    if warm_disk_per_q > 1.0 {
        eprintln!("   [URGENT] Disk fetches on warm path ({warm_disk_per_q:.0}/q) — L0/vec cache not warm enough");
    } else {
        eprintln!("   [OK] Warm path: {warm_disk_per_q:.1} disk fetches/q (cache is working)");
    }
    if warm_dist_per_q > 1000.0 {
        eprintln!("   [HIGH ROI] Distance calls {warm_dist_per_q:.0}/q × dim128 scalar → SIMD could 4–8× speedup");
    }
    if warm_l0_per_q + warm_vec_per_q > 500.0 {
        eprintln!(
            "   [MEDIUM ROI] {:.0} Vec clones/q → zero-copy arena could save {}–{} µs",
            warm_l0_per_q + warm_vec_per_q,
            (est_l0_clone_us + est_vec_clone_us) as i64,
            (est_l0_clone_us + est_vec_clone_us + 50.0) as i64
        );
    }
    eprintln!("═══════════════════════════════════════════════════════════════════════");
    eprintln!();

    // This test always passes — it's a profiling tool, not a gate.
    // The recall and latency gates are separate tests.
    let _ = (corpus_vecs, cold_us, warm_us);
}

/// Recall@10 gate at 2k×dim128: ensures recall stays ≥ 0.90.
/// This is the regression gate for item 92 levers.
#[test]
fn hnsw_recall_gate_2k() {
    let n: usize = std::env::var("ITEM92_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000);

    let (engine, _dir, corpus_vecs, _query_sqls) = build_engine(n);

    let n_recall_queries = 10usize;
    let mut total_hits = 0usize;
    let total = n_recall_queries * K;

    for qi in 0..n_recall_queries {
        let query_vec = rand_vec_128((n as u64) + 200_000 + qi as u64);

        // Brute-force top-K.
        let mut exact: Vec<(f64, usize)> = corpus_vecs
            .iter()
            .enumerate()
            .map(|(i, v)| (euclidean_dist_sq(&query_vec, v), i))
            .collect();
        exact.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let ground_truth: HashSet<i64> = exact.iter().take(K).map(|(_, i)| *i as i64).collect();

        // HNSW via engine.
        let sql = near_sql(&query_vec, K);
        let w = engine.begin().unwrap();
        let results = engine.execute_sql(w, &sql).unwrap();
        engine.commit(w).unwrap();

        let hnsw_ids: HashSet<i64> = match &results[0] {
            unidb::SqlResult::Rows { rows, .. } => rows
                .iter()
                .filter_map(|r| match &r[0] {
                    Literal::Int(n) => Some(*n),
                    _ => None,
                })
                .collect(),
            _ => HashSet::new(),
        };

        total_hits += ground_truth.intersection(&hnsw_ids).count();
    }

    let recall = total_hits as f64 / total as f64;
    eprintln!("[item92] recall@{K} at {n}×dim{DIM}: {recall:.3} (gate ≥ 0.90)");
    assert!(
        recall >= 0.90,
        "recall@{K} at {n}×dim{DIM} = {recall:.3} < 0.90 — HNSW quality regression (item 92)"
    );
}
