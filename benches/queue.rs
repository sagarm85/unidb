// Event queue benchmarks (M4.d). Baseline comparison: Postgres-as-queue via
// `SELECT ... FOR UPDATE SKIP LOCKED` — the standard "poor man's queue"
// idiom, run separately via `psql`/`EXPLAIN ANALYZE` against a local,
// isolated database and recorded in PROGRESS.md (same discipline as M2.d's
// pgvector and M3.d's adjacency-table comparisons — Postgres numbers are
// not folded into this Rust harness).
// Run with: cargo bench --bench queue

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use tempfile::tempdir;
use unidb::Engine;

fn insert_n(engine: &mut Engine, n: i64) {
    let xid = engine.begin().unwrap();
    for i in 0..n {
        engine
            .execute_sql(xid, &format!("INSERT INTO t (id) VALUES ({i})"))
            .unwrap();
    }
    engine.commit(xid).unwrap();
}

fn new_table(engine: &mut Engine) {
    let xid = engine.begin().unwrap();
    engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
    engine.commit(xid).unwrap();
}

/// Isolates event capture's actual cost: same INSERT workload, events
/// disabled vs. enabled. The delta is exactly one extra synchronous
/// `heap.insert` + `record_undo` per row (M4.a's `send_event_capture`).
fn bench_event_capture_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_capture_overhead");

    group.bench_function("insert_100_events_disabled", |b| {
        b.iter(|| {
            let dir = tempdir().unwrap();
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            new_table(&mut engine);
            insert_n(&mut engine, 100);
        });
    });

    group.bench_function("insert_100_events_enabled", |b| {
        b.iter(|| {
            let dir = tempdir().unwrap();
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            new_table(&mut engine);
            engine.enable_events("t").unwrap();
            insert_n(&mut engine, 100);
        });
    });

    group.finish();
}

/// `poll_events` has no predicate pushdown (queue/mod.rs's module doc,
/// lib.rs's `poll_events` doc comment) — cost scales with `__events__`'s
/// total row count, not with consumer lag or `limit`. This benchmark
/// quantifies that relationship directly rather than just asserting it.
fn bench_poll_events_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("poll_events_latency");
    for n in [100i64, 1_000, 5_000] {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        new_table(&mut engine);
        engine.enable_events("t").unwrap();
        insert_n(&mut engine, n);

        let xid = engine.begin().unwrap();
        group.bench_with_input(BenchmarkId::new("poll_limit_10", n), &n, |b, _| {
            b.iter(|| engine.poll_events(xid, "bench-consumer", 10).unwrap());
        });
        engine.commit(xid).unwrap();
    }
    group.finish();
}

/// `vacuum_events` cost as a function of reclaimed-row count — the actual
/// lever for `poll_events`'s total-table-size cost, since it's the only
/// thing that shrinks `__events__` (M4.c; never automatic).
fn bench_vacuum_events_cost(c: &mut Criterion) {
    let mut group = c.benchmark_group("vacuum_events_cost");
    for n in [100i64, 1_000, 5_000] {
        group.bench_with_input(BenchmarkId::new("reclaim_all", n), &n, |b, _| {
            b.iter_batched(
                || {
                    let dir = tempdir().unwrap();
                    let mut engine = Engine::open(dir.path(), 0).unwrap();
                    new_table(&mut engine);
                    engine.enable_events("t").unwrap();
                    insert_n(&mut engine, n);

                    let ack_xid = engine.begin().unwrap();
                    let all = engine.poll_events(ack_xid, "c", n as usize).unwrap();
                    engine
                        .ack_events(ack_xid, "c", all.last().unwrap().seq)
                        .unwrap();
                    engine.commit(ack_xid).unwrap();
                    (dir, engine)
                },
                |(dir, engine)| {
                    let xid = engine.begin().unwrap();
                    let reclaimed = engine.vacuum_events(xid).unwrap();
                    engine.commit(xid).unwrap();
                    drop(dir);
                    reclaimed
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_event_capture_overhead, bench_poll_events_latency, bench_vacuum_events_cost
}
criterion_main!(benches);
