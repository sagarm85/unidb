// Item 115 — Step-0 attribution for the ONE-SHOT (cold) filtered SELECT.
//
// The Docker bench (Table 3) times exactly ONE `execute_sql` of a
// never-before-seen statement text against a 200k-row table: a plan-cache
// miss plus whatever other first-execution costs exist. perf_item109 measures
// the WARM path; this probe decomposes the one-shot premium the bench pays.
//
// Method — three sample sets over the same 200k-row table:
//   first     — the very first filtered SELECT the engine ever runs
//               (global one-time costs land here: first parallel dispatch,
//               lazily-initialised statics, allocator growth)
//   plan-miss — further queries with DISTINCT statement texts (each a
//               plan-cache miss; pages/executor warm). Median isolates the
//               steady per-new-statement premium.
//   warm      — one text repeated (plan-cache hit). The floor.
//
// Phase split per set via the permanent Q115 statement timers
// (parse+plan / RLS rewrite / execute) and the Q109 executor counters
// (leaf walk / candidate resolution) inside the execute phase.
//
// Run:  ITEM115_N=200000 cargo test --release --test perf_item115 -- --nocapture

use std::sync::atomic::Ordering as AtomicOrd;
use std::time::Instant;
use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::sql::parallel_scan::{
    item109_reset, item115_reset, Q109_LEAF_NANOS, Q109_RESOLVE_NANOS, Q115_EXEC_NANOS,
    Q115_PARSE_NANOS, Q115_RLS_NANOS,
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

struct Sample {
    total_us: f64,
    parse_us: f64,
    rls_us: f64,
    exec_us: f64,
    leaf_us: f64,
    resolve_us: f64,
}

fn run_one(engine: &Engine, sql: &str) -> Sample {
    item115_reset();
    item109_reset();
    let x = engine.begin().unwrap();
    let start = Instant::now();
    let res = engine.execute_sql(x, sql).unwrap();
    let total_us = start.elapsed().as_secs_f64() * 1e6;
    engine.commit(x).unwrap();
    assert!(!res.is_empty());
    Sample {
        total_us,
        parse_us: Q115_PARSE_NANOS.load(AtomicOrd::Relaxed) as f64 / 1e3,
        rls_us: Q115_RLS_NANOS.load(AtomicOrd::Relaxed) as f64 / 1e3,
        exec_us: Q115_EXEC_NANOS.load(AtomicOrd::Relaxed) as f64 / 1e3,
        leaf_us: Q109_LEAF_NANOS.load(AtomicOrd::Relaxed) as f64 / 1e3,
        resolve_us: Q109_RESOLVE_NANOS.load(AtomicOrd::Relaxed) as f64 / 1e3,
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn med(samples: &[Sample], f: impl Fn(&Sample) -> f64) -> f64 {
    median(samples.iter().map(f).collect())
}

fn print_set(
    name: &str,
    s_total: f64,
    s_parse: f64,
    s_rls: f64,
    s_exec: f64,
    s_leaf: f64,
    s_res: f64,
) {
    let inner_other = (s_exec - s_leaf - s_res).max(0.0);
    let outer = (s_total - s_parse - s_rls - s_exec).max(0.0);
    eprintln!(
        " {name:<10} total {s_total:>8.1} µs · parse+plan {s_parse:>7.1} · rls {s_rls:>5.1} · exec {s_exec:>8.1} (leaf {s_leaf:>6.1} + resolve {s_res:>7.1} + other {inner_other:>6.1}) · outer {outer:>5.1}"
    );
}

#[test]
fn one_shot_filtered_select_phase_split() {
    let n: i64 = std::env::var("ITEM115_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);
    let hi = n / 40; // 5% of the FIRST 200k-row bench table's k-range shape

    let dir = tempdir().unwrap();
    let engine = Engine::open_with_pool_capacity(dir.path(), 0, 2_000_000).unwrap();
    engine.set_deferred_sync(true);
    build(&engine, n);

    // ITEM115_PREWARM=1: run one filtered SELECT against a tiny UNRELATED
    // table before `first`. If `first` then collapses to plan-miss level, the
    // one-shot premium is global (per-engine lazy init → open-time warmup
    // fixes it); if it stays high, it is per-table/per-index state.
    // ITEM115_PREWARM=2: additionally prewarm table t itself on a DISJOINT
    // key range (different heap pages). Distinguishes per-table lazy state
    // (would collapse `first`) from per-page first-touch (would not).
    if std::env::var("ITEM115_PREWARM").as_deref() == Ok("2") {
        let x = engine.begin().unwrap();
        let lo = n - hi;
        let _ = engine
            .execute_sql(
                x,
                &format!("SELECT id, body FROM t WHERE k >= {lo} AND k < {n}"),
            )
            .unwrap();
        engine.commit(x).unwrap();
    }
    if std::env::var("ITEM115_PREWARM").as_deref() == Ok("1")
        || std::env::var("ITEM115_PREWARM").as_deref() == Ok("2")
    {
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE w (id INT, k INT, body TEXT)")
            .unwrap();
        engine
            .execute_sql(x, "CREATE INDEX w_k ON w USING BTREE (k)")
            .unwrap();
        for i in 0..200 {
            engine
                .execute_sql(
                    x,
                    &format!("INSERT INTO w (id, k, body) VALUES ({i}, {i}, 'x')"),
                )
                .unwrap();
        }
        engine.commit(x).unwrap();
        let x = engine.begin().unwrap();
        let _ = engine
            .execute_sql(x, "SELECT id, body FROM w WHERE k >= 0 AND k < 100")
            .unwrap();
        engine.commit(x).unwrap();
    }

    // ── first: the engine's first-ever filtered SELECT ──────────────────────
    let first = run_one(
        &engine,
        &format!("SELECT id, body FROM t WHERE k >= 0 AND k < {hi}"),
    );

    // ── plan-miss: distinct texts, everything else warm ─────────────────────
    const MISS_RUNS: i64 = 15;
    let miss: Vec<Sample> = (1..=MISS_RUNS)
        .map(|j| {
            run_one(
                &engine,
                &format!("SELECT id, body FROM t WHERE k >= {j} AND k < {}", hi + j),
            )
        })
        .collect();

    // ── warm: one text repeated (plan-cache hit) ────────────────────────────
    let warm_sql = format!("SELECT id, body FROM t WHERE k >= 0 AND k < {hi}");
    let warm: Vec<Sample> = (0..20).map(|_| run_one(&engine, &warm_sql)).collect();

    eprintln!();
    eprintln!("═══════════════════════════════════════════════════════════════════════════════");
    eprintln!(" Item 115 Step-0 — one-shot filtered SELECT phase split ({n} rows, 5% sel)");
    eprintln!("═══════════════════════════════════════════════════════════════════════════════");
    print_set(
        "first",
        first.total_us,
        first.parse_us,
        first.rls_us,
        first.exec_us,
        first.leaf_us,
        first.resolve_us,
    );
    print_set(
        "plan-miss",
        med(&miss, |s| s.total_us),
        med(&miss, |s| s.parse_us),
        med(&miss, |s| s.rls_us),
        med(&miss, |s| s.exec_us),
        med(&miss, |s| s.leaf_us),
        med(&miss, |s| s.resolve_us),
    );
    print_set(
        "warm",
        med(&warm, |s| s.total_us),
        med(&warm, |s| s.parse_us),
        med(&warm, |s| s.rls_us),
        med(&warm, |s| s.exec_us),
        med(&warm, |s| s.leaf_us),
        med(&warm, |s| s.resolve_us),
    );
    eprintln!("───────────────────────────────────────────────────────────────────────────────");
    eprintln!(
        " one-shot premium (first − warm total)      : {:>8.1} µs",
        first.total_us - med(&warm, |s| s.total_us)
    );
    eprintln!(
        " global first-use (first − plan-miss total) : {:>8.1} µs",
        first.total_us - med(&miss, |s| s.total_us)
    );
    eprintln!(
        " per-new-statement (plan-miss − warm total) : {:>8.1} µs",
        med(&miss, |s| s.total_us) - med(&warm, |s| s.total_us)
    );
    eprintln!("═══════════════════════════════════════════════════════════════════════════════");
    eprintln!();
}
