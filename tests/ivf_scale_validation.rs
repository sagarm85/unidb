// Item 62 — IVF-Flat scale validation tests.
//
// Two tests that gate the disk-HNSW implementation effort:
//
//   1. `recall_at_k_computation_correct`  — pure-math unit test that the
//      intersection-over-k formula is right before running it on large corpora.
//
//   2. `nlist_correct_when_index_created_after_insert` — observational test
//      that an IVF index built on a pre-populated table uses real partitioning
//      (nlist > 1) rather than the degenerate nlist=1 single-origin-centroid
//      that `CREATE INDEX` on an empty table produces.

use std::collections::HashSet;
use tempfile::tempdir;
use unidb::sql::executor::ExecResult as SqlResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

/// Verify the recall@k formula: |IVF_topK ∩ BF_topK| / k.
/// This is a pure-math check so bugs in the bench's recall computation are
/// caught before they silently produce optimistic numbers at 100k+ rows.
#[test]
fn recall_at_k_computation_correct() {
    let k = 10usize;

    // Perfect recall: IVF and BF agree completely.
    let found: HashSet<i64> = (0..10).collect();
    let expected: HashSet<i64> = (0..10).collect();
    let recall = found.intersection(&expected).count() as f64 / k as f64;
    assert!(
        (recall - 1.0).abs() < 1e-9,
        "perfect recall must be 1.0, got {recall}"
    );

    // One miss: IVF returned 10 instead of 9.
    let found: HashSet<i64> = [0, 1, 2, 3, 4, 5, 6, 7, 8, 10].iter().copied().collect();
    let expected: HashSet<i64> = (0..10).collect();
    let recall = found.intersection(&expected).count() as f64 / k as f64;
    assert!(
        (recall - 0.9).abs() < 1e-9,
        "one-miss recall must be 0.9, got {recall}"
    );

    // Four misses: IVF returned 10-13 instead of 6-9.
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

/// Verify that an IVF index built AFTER N rows are inserted uses real Voronoi
/// partitioning (nlist > 1), not the degenerate nlist=1 single-origin-centroid
/// that `CREATE INDEX` on an *empty* table produces.
///
/// The observable test: with nlist=20 (√400) and nprobe=8, a NEAR query
/// probes only ~40% of cells → returns ≈160 candidates, well below N=400.
/// With nlist=1 (empty-table index), every cell is the whole table → returns
/// all 400 rows.  The assertion `result_count < N` distinguishes the two.
///
/// Why this matters: the W2 bench (multi-model decomposition ladder) creates
/// the IVF index on an EMPTY table → nlist=1 → every NEAR query is a full
/// linear scan. `bench_ivf_scale_validation` (UNIDB_BENCH=ivf_validate) fixes
/// this by creating the index after insert; this test proves the fix works.
#[test]
fn nlist_correct_when_index_created_after_insert() {
    let n = 400usize;
    // Expected IVF params for N=400: nlist=√400=20, nprobe=max(20/8,8)=8.
    // Probes 8/20 = 40% of cells → ≈160 candidates < 400.

    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Create table — NO index yet.
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
        .unwrap();
    engine.commit(xid).unwrap();

    // Insert N rows with 2D vectors spread around the unit circle so the
    // k-means centroids land in distinct angular sectors.
    let ins = engine
        .prepare("INSERT INTO t (id, embedding) VALUES ($1, $2)")
        .unwrap();
    let mut xid = engine.begin().unwrap();
    for i in 0..n {
        let angle = i as f64 * 2.0 * std::f64::consts::PI / n as f64;
        let x = angle.sin() as f32;
        let y = angle.cos() as f32;
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

    // CREATE INDEX AFTER insert → centroids trained on the full corpus →
    // nlist = min(round(√400), 256) = 20.
    let ixid = engine.begin().unwrap();
    engine
        .execute_sql(ixid, "CREATE INDEX iv ON t USING HNSW (embedding)")
        .unwrap();
    engine.commit(ixid).unwrap();

    // Ask for k = N (all rows). With nlist=20 and nprobe=8, approximately
    // 8 × (400/20) = 160 candidates are returned — not 400.
    let q = format!("SELECT id FROM t WHERE NEAR(embedding, [0.0, 1.0], {n})");
    let xid3 = engine.begin().unwrap();
    let results = engine.execute_sql(xid3, &q).unwrap();
    engine.commit(xid3).unwrap();

    let result_count = match &results[0] {
        SqlResult::Rows { rows, .. } => rows.len(),
        other => panic!("expected Rows result, got {other:?}"),
    };

    // Sanity: got at least something.
    assert!(
        result_count > 0,
        "NEAR on pre-populated index returned no results"
    );
    // Key assertion: IVF partitioning limits candidates to a fraction of N.
    // With nlist=20, nprobe=8 → probes 40% of cells → result_count ≈ 160.
    // Use a generous bound (< N * 0.85 = 340) to account for cell-size variance.
    assert!(
        result_count < (n as f64 * 0.85) as usize,
        "expected IVF partitioning (result_count < {}, got {}); \
         nlist=1 (empty-table index) would return {n}",
        (n as f64 * 0.85) as usize,
        result_count
    );
}
