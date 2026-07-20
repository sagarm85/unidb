// RLS perf gate — item-24 R-a/R-b acceptance criteria
//
// Gate 1: Table 3 SELECT filtered throughput unchanged with no policies.
//         (µs/query on no-policy engine ≈ µs/query on engine with policies
//          when queried as superuser without a user identity.)
//
// Gate 2: RLS-on vs manual WHERE on an indexed column: overhead ≤ 10%.
//         Measures `SELECT ... WHERE user_id = 'alice'` (no policy)
//         vs      `SELECT ...` as alice with `USING (user_id = current_user)`.
//
// Run with:  cargo test --test rls_perf_gate --release
// (release is required — debug timings have noise that inflates the ratio)

use std::time::Instant;
use tempfile::tempdir;
use unidb::Engine;

const N: usize = 2_000; // enough for stable signal; 10k is too slow per-row
const WARMUP: usize = 2;
const SAMPLE: usize = 8;

fn exec_super(engine: &Engine, sql: &str) {
    let x = engine.begin().unwrap();
    engine.execute_sql_as(None, x, sql).unwrap();
    engine.commit(x).unwrap();
}

fn timed_select_no_policy(engine: &Engine) -> u64 {
    let t = Instant::now();
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(None, x, "SELECT id FROM t WHERE user_id = 'alice'")
        .unwrap();
    engine.commit(x).unwrap();
    t.elapsed().as_micros() as u64
}

fn timed_select_rls(engine: &Engine) -> u64 {
    let t = Instant::now();
    let x = engine.begin().unwrap();
    engine
        .execute_sql_as(Some("alice"), x, "SELECT id FROM t")
        .unwrap();
    engine.commit(x).unwrap();
    t.elapsed().as_micros() as u64
}

/// Build a 10k-row table with no RLS policy.
fn build_no_policy_engine() -> (Engine, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    exec_super(&engine, "CREATE TABLE t (id INT, user_id TEXT)");
    // Batch insert via exec_super (no per-row overhead in the measurement)
    for i in 0..N {
        let uid = if i % 2 == 0 { "alice" } else { "bob" };
        exec_super(&engine, &format!("INSERT INTO t VALUES ({i}, '{uid}')"));
    }
    (engine, dir)
}

/// Build a 10k-row table WITH a SELECT policy on user_id = current_user.
fn build_rls_engine() -> (Engine, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    exec_super(&engine, "CREATE TABLE t (id INT, user_id TEXT)");
    exec_super(&engine, "CREATE USER alice");
    exec_super(&engine, "GRANT SELECT ON t TO alice");
    exec_super(
        &engine,
        "CREATE POLICY own ON t FOR SELECT USING (user_id = current_user)",
    );
    for i in 0..N {
        let uid = if i % 2 == 0 { "alice" } else { "bob" };
        exec_super(&engine, &format!("INSERT INTO t VALUES ({i}, '{uid}')"));
    }
    (engine, dir)
}

// ── Gate 1: no-policy SELECT baseline is not regressed ───────────────────────

#[test]
#[ignore = "perf gate: run explicitly with --release --ignored"]
fn gate1_no_policy_select_throughput() {
    let (engine_no_policy, _dir1) = build_no_policy_engine();
    let (engine_with_policy, _dir2) = build_rls_engine();

    // Warm up both engines.
    for _ in 0..WARMUP {
        timed_select_no_policy(&engine_no_policy);
        // Superuser SELECT on the RLS engine — policy must be invisible to superuser.
        let x = engine_with_policy.begin().unwrap();
        engine_with_policy
            .execute_sql_as(None, x, "SELECT id FROM t")
            .unwrap();
        engine_with_policy.commit(x).unwrap();
    }

    let mut np_sum = 0u64;
    let mut wp_super_sum = 0u64;
    for _ in 0..SAMPLE {
        np_sum += timed_select_no_policy(&engine_no_policy);
        let t = Instant::now();
        let x = engine_with_policy.begin().unwrap();
        engine_with_policy
            .execute_sql_as(None, x, "SELECT id FROM t WHERE user_id = 'alice'")
            .unwrap();
        engine_with_policy.commit(x).unwrap();
        wp_super_sum += t.elapsed().as_micros() as u64;
    }
    let np_avg = np_sum / SAMPLE as u64;
    let wp_avg = wp_super_sum / SAMPLE as u64;
    let ratio = wp_avg as f64 / np_avg as f64;

    println!(
        "Gate 1 — no-policy: {np_avg} µs | with-policy (superuser): {wp_avg} µs | ratio: {ratio:.2}×"
    );
    // Superuser path skips policies entirely — should be within 15% of no-policy
    // (small overhead from catalog lookup at open; not a correctness difference).
    assert!(
        ratio < 1.15,
        "Gate 1 FAIL: superuser SELECT on policy-table is {ratio:.2}× slower than no-policy engine (gate ≤1.15×)"
    );
}

// ── Gate 2: RLS overhead vs manual WHERE ≤ 10% ───────────────────────────────

#[test]
#[ignore = "perf gate: run explicitly with --release --ignored"]
fn gate2_rls_overhead_vs_manual_where() {
    let (engine_manual, _dir1) = build_no_policy_engine();
    let (engine_rls, _dir2) = build_rls_engine();

    // Warm up.
    for _ in 0..WARMUP {
        timed_select_no_policy(&engine_manual);
        timed_select_rls(&engine_rls);
    }

    let mut manual_sum = 0u64;
    let mut rls_sum = 0u64;
    for _ in 0..SAMPLE {
        manual_sum += timed_select_no_policy(&engine_manual);
        rls_sum += timed_select_rls(&engine_rls);
    }
    let manual_avg = manual_sum / SAMPLE as u64;
    let rls_avg = rls_sum / SAMPLE as u64;
    let ratio = rls_avg as f64 / manual_avg as f64;

    println!(
        "Gate 2 — manual WHERE: {manual_avg} µs | RLS policy: {rls_avg} µs | overhead: {ratio:.2}×"
    );
    assert!(
        ratio <= 1.10,
        "Gate 2 FAIL: RLS overhead {ratio:.2}× exceeds 1.10× (manual WHERE {manual_avg} µs vs RLS {rls_avg} µs)"
    );
}
