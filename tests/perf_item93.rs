// Item 93 — HNSW L0 arena layout: warm latency gate + arena correctness.
//
// ## Purpose
//
// Validates that the L0Arena (flat contiguous Vec<i64> slab) correctly serves
// warm NEAR queries with:
//   1. Recall@10 ≥ 0.90 at 2k×dim128 (regression gate — same as item 92).
//   2. Zero disk fetches on the warm path (all neighbours served from arena).
//   3. Warm latency ≤ 800 µs at 2k×dim128 in release mode (item 93 target is
//      ≤ 600 µs at 10k; Docker bench is the definitive gate for that number).
//
// ## Running
//
// Debug (CI gate, fast):
//   cargo test --test perf_item93 -- --nocapture
//
// Release (latency measurement, ITEM93_N=10000 for full target):
//   ITEM93_N=10000 cargo test --release --test perf_item93 -- --nocapture

use std::collections::HashSet;
use std::sync::atomic::Ordering as AtomicOrd;
use std::time::Instant;
use tempfile::tempdir;
use unidb::hnsw_index::{Q_DISK_FETCHES, Q_L0_CACHE_HITS};
use unidb::sql::logical::Literal;
use unidb::Engine;

const DIM: usize = 128;
const K: usize = 10;

/// Deterministic 128-dim vector from a seed.
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

/// Build an engine + HNSW index with `n` rows.
/// Returns (engine, _dir, corpus_vecs, query_sqls).
fn build_engine(n: usize) -> (Engine, tempfile::TempDir, Vec<Vec<f32>>, Vec<String>) {
    let dir = tempdir().unwrap();
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

    // Build HNSW index (triggers prefetch_caches → populates arena).
    let t_idx = Instant::now();
    let ixid = engine.begin().unwrap();
    engine
        .execute_sql(ixid, "CREATE INDEX iv ON t USING HNSW (embedding)")
        .unwrap();
    engine.commit(ixid).unwrap();
    let idx_ms = t_idx.elapsed().as_secs_f64() * 1e3;
    eprintln!("[item93] CREATE INDEX on {n} rows: {idx_ms:.0} ms");

    let n_queries = 20usize;
    let query_vecs: Vec<Vec<f32>> = (0..n_queries)
        .map(|i| rand_vec_128((n as u64) + 100_000 + i as u64))
        .collect();
    let query_sqls: Vec<String> = query_vecs.iter().map(|q| near_sql(q, K)).collect();

    (engine, dir, corpus_vecs, query_sqls)
}

/// Gate test: recall@10 ≥ 0.90 and zero disk fetches on warm path.
///
/// This test validates two item-93 correctness properties:
///   1. The arena correctly stores and retrieves neighbour lists (recall gate).
///   2. On the warm path (post-prefetch), all L0 lookups are arena hits (disk = 0).
#[test]
fn hnsw_arena_recall_and_zero_disk() {
    let n: usize = std::env::var("ITEM93_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000);

    let (engine, _dir, corpus_vecs, query_sqls) = build_engine(n);
    let n_queries = query_sqls.len();

    // Warm-up: 5 queries to ensure caches are fully populated.
    for sql in query_sqls.iter().take(5.min(n_queries)) {
        let w = engine.begin().unwrap();
        engine.execute_sql(w, sql).unwrap();
        engine.commit(w).unwrap();
    }

    // ── Warm queries: measure latency + counters ─────────────────────────────
    let warm_start = 5.min(n_queries);
    let warm_count = (n_queries - warm_start).max(1);

    unidb::hnsw_index::reset_query_counters();
    let t_warm = Instant::now();
    for sql in query_sqls.iter().take(n_queries).skip(warm_start) {
        let w = engine.begin().unwrap();
        engine.execute_sql(w, sql).unwrap();
        engine.commit(w).unwrap();
    }
    let total_warm_us = t_warm.elapsed().as_secs_f64() * 1e6;
    let warm_us = total_warm_us / warm_count as f64;

    let warm_l0_hits = Q_L0_CACHE_HITS.load(AtomicOrd::Relaxed);
    let warm_disk = Q_DISK_FETCHES.load(AtomicOrd::Relaxed);

    eprintln!();
    eprintln!("═══════════════════════════════════════════════════════");
    eprintln!(" Item 93 Arena Gate — {n}×dim{DIM}, k={K}");
    eprintln!("═══════════════════════════════════════════════════════");
    eprintln!(" Warm latency (avg {warm_count} queries): {warm_us:.1} µs");
    eprintln!(" L0 arena hits   : {warm_l0_hits}");
    eprintln!(" Disk fetches    : {warm_disk}  (must be 0 on warm path)");
    eprintln!("───────────────────────────────────────────────────────");

    // ── Recall@10 gate ───────────────────────────────────────────────────────

    let test_query = rand_vec_128(n as u64 + 999_999);
    let query_sql = near_sql(&test_query, K);

    // Brute-force ground truth.
    let mut exact: Vec<(f64, usize)> = corpus_vecs
        .iter()
        .enumerate()
        .map(|(i, v)| (euclidean_dist_sq(&test_query, v), i))
        .collect();
    exact.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let ground_truth_ids: HashSet<usize> = exact.iter().take(K).map(|(_, i)| *i).collect();

    // NEAR query result.
    let w = engine.begin().unwrap();
    let result = engine.execute_sql(w, &query_sql).unwrap();
    engine.commit(w).unwrap();

    let returned_ids: HashSet<usize> = result
        .iter()
        .filter_map(|r| match r {
            unidb::SqlResult::Rows { rows, .. } => Some(rows.iter().filter_map(|row| {
                if let Some(Literal::Int(id)) = row.first() {
                    Some(*id as usize)
                } else {
                    None
                }
            })),
            _ => None,
        })
        .flatten()
        .collect();

    let hits = ground_truth_ids.intersection(&returned_ids).count();
    let recall = hits as f64 / K as f64;
    eprintln!(" Recall@10: {recall:.3} (need ≥ 0.90)");
    eprintln!("═══════════════════════════════════════════════════════");

    // ── Assertions ───────────────────────────────────────────────────────────

    // Arena hit gate: warm queries must have zero disk fetches.
    // (All neighbours come from arena_data[] without any DiskBTree lookup.)
    assert_eq!(
        warm_disk, 0,
        "Item 93: warm path still fetching from disk ({warm_disk} disk fetches); \
         arena not serving all L0 neighbours"
    );

    // Arena is serving neighbours (not falling through to insert path).
    assert!(
        warm_l0_hits > 0,
        "Item 93: L0 arena hit counter is zero — arena may not be populated"
    );

    // Recall gate.
    assert!(
        recall >= 0.90,
        "Item 93: recall@10 = {recall:.3} (need ≥ 0.90) — arena may be storing wrong neighbours"
    );

    // Latency gate: only enforced in release mode (debug is ~10-30× slower due to
    // unoptimized code).  The item-93 Docker target is ≤ 600 µs at 10k rows in
    // release mode on Linux/ARM; this gate checks a conservative 800 µs at 2k
    // to flag regressions while remaining robust on CI.
    #[cfg(not(debug_assertions))]
    if n <= 2_000 {
        assert!(
            warm_us <= 800.0,
            "Item 93: warm NEAR latency {warm_us:.1} µs > 800 µs at {n} rows (release gate)"
        );
    }
}
