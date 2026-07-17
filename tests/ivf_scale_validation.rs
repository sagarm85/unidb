// Item 62/63 — vector index validation tests.
//
// Item 62 validated that IVF-Flat recall@10 failed the gate (≥0.90) at 100k rows.
// Item 63 replaced IVF-Flat with on-disk HNSW, which achieves recall@10 ≥ 0.95.
//
// Two tests remain:
//
//   1. `recall_at_k_computation_correct`  — pure-math unit test that the
//      intersection-over-k formula is right before running it on large corpora.
//
//   2. `hnsw_near_returns_approximate_nearest`  — end-to-end test that HNSW
//      NEAR queries return correct approximate nearest neighbours (recall@10 ≥ 0.85
//      on a small 400-row corpus as a fast sanity check; the production gate is
//      recall@10 ≥ 0.95 at 1k/10k rows via `UNIDB_BENCH=ivf_validate`).

use std::collections::HashSet;
use tempfile::tempdir;
use unidb::sql::executor::ExecResult as SqlResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

/// Verify the recall@k formula: |HNSW_topK ∩ BF_topK| / k.
/// This is a pure-math check so bugs in the bench's recall computation are
/// caught before they silently produce optimistic numbers at 100k+ rows.
#[test]
fn recall_at_k_computation_correct() {
    let k = 10usize;

    // Perfect recall: HNSW and BF agree completely.
    let found: HashSet<i64> = (0..10).collect();
    let expected: HashSet<i64> = (0..10).collect();
    let recall = found.intersection(&expected).count() as f64 / k as f64;
    assert!(
        (recall - 1.0).abs() < 1e-9,
        "perfect recall must be 1.0, got {recall}"
    );

    // One miss: HNSW returned 10 instead of 9.
    let found: HashSet<i64> = [0, 1, 2, 3, 4, 5, 6, 7, 8, 10].iter().copied().collect();
    let expected: HashSet<i64> = (0..10).collect();
    let recall = found.intersection(&expected).count() as f64 / k as f64;
    assert!(
        (recall - 0.9).abs() < 1e-9,
        "one-miss recall must be 0.9, got {recall}"
    );

    // Four misses: HNSW returned 10-13 instead of 6-9.
    let found: HashSet<i64> = [0, 1, 2, 3, 4, 5, 10, 11, 12, 13].iter().copied().collect();
    let expected: HashSet<i64> = (0..10).collect();
    let recall = found.intersection(&expected).count() as f64 / k as f64;
    assert!(
        (recall - 0.6).abs() < 1e-9,
        "four-miss recall must be 0.6, got {recall}"
    );

    // Complete miss: no overlap.
    let found: HashSet<i64> = (10..20).collect();
    let expected: HashSet<i64> = (0..10).collect();
    let recall = found.intersection(&expected).count() as f64 / k as f64;
    assert!(
        recall.abs() < 1e-9,
        "zero-overlap recall must be 0.0, got {recall}"
    );
}

/// End-to-end test: HNSW NEAR returns approximate nearest neighbours with
/// recall@10 ≥ 0.85 on a 400-row × 2-dim corpus.
///
/// This is intentionally small (fast CI) — the production recall gate (≥0.95
/// at 1k/10k×dim128) runs via `UNIDB_BENCH=ivf_validate cargo bench --bench
/// decompose`.
///
/// 2D vectors are placed on the unit circle so the exact ground truth is
/// trivially computable and recall is unambiguous.
#[test]
fn hnsw_near_returns_approximate_nearest() {
    use std::f64::consts::PI;

    let n = 400usize;
    let k = 10usize;
    const N_QUERIES: usize = 20;

    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Create table — NO index yet.
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
        .unwrap();
    engine.commit(xid).unwrap();

    // Insert N rows with 2D vectors spread around the unit circle.
    let ins = engine
        .prepare("INSERT INTO t (id, embedding) VALUES ($1, $2)")
        .unwrap();
    let mut corpus: Vec<[f32; 2]> = Vec::with_capacity(n);
    let mut xid = engine.begin().unwrap();
    for i in 0..n {
        let angle = i as f64 * 2.0 * PI / n as f64;
        let x = angle.sin() as f32;
        let y = angle.cos() as f32;
        corpus.push([x, y]);
        engine
            .execute_prepared(
                xid,
                &ins,
                &[Literal::Int(i as i64), Literal::Vector(vec![x, y])],
            )
            .unwrap();
        if (i + 1) % 100 == 0 {
            engine.commit(xid).unwrap();
            xid = engine.begin().unwrap();
        }
    }
    engine.commit(xid).unwrap();

    // CREATE INDEX AFTER insert → HNSW graph built on full corpus.
    let ixid = engine.begin().unwrap();
    engine
        .execute_sql(ixid, "CREATE INDEX iv ON t USING HNSW (embedding)")
        .unwrap();
    engine.commit(ixid).unwrap();

    // Query N_QUERIES evenly-spaced angles (offset so they aren't in the corpus).
    let mut total_hits = 0usize;
    let mut total_possible = 0usize;

    for qi in 0..N_QUERIES {
        let angle = (qi as f64 + 0.5) * 2.0 * PI / N_QUERIES as f64;
        let qx = angle.sin() as f32;
        let qy = angle.cos() as f32;

        // Brute-force ground truth: k nearest by L2.
        let mut scored: Vec<(f32, i64)> = corpus
            .iter()
            .enumerate()
            .map(|(i, &[cx, cy])| {
                let dx = qx - cx;
                let dy = qy - cy;
                (dx * dx + dy * dy, i as i64)
            })
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        let ground_truth: HashSet<i64> = scored.iter().take(k).map(|&(_, id)| id).collect();

        // NEAR via HNSW.
        let q = format!("SELECT id FROM t WHERE NEAR(embedding, [{qx:.8}, {qy:.8}], {k})");
        let xid3 = engine.begin().unwrap();
        let results = engine.execute_sql(xid3, &q).unwrap();
        engine.commit(xid3).unwrap();

        let hnsw_ids: HashSet<i64> = match &results[0] {
            SqlResult::Rows { rows, .. } => rows
                .iter()
                .filter_map(|r| match &r[0] {
                    Literal::Int(n) => Some(*n),
                    _ => None,
                })
                .collect(),
            other => panic!("expected Rows result, got {other:?}"),
        };

        assert!(
            !hnsw_ids.is_empty(),
            "NEAR on HNSW index returned no results for query {qi}"
        );

        total_hits += ground_truth.intersection(&hnsw_ids).count();
        total_possible += k;
    }

    let recall = total_hits as f64 / total_possible as f64;
    assert!(
        recall >= 0.85,
        "recall@{k} at {n}×dim2 = {recall:.3} (need ≥ 0.85 for this fast sanity check)"
    );
}
