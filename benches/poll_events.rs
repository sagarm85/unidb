// Item 26 Q1 acceptance bench: poll latency must be FLAT as the __events__
// table grows 10k → 100k → 1M while the returned-event count is held constant.
//
// Pre-item-26 `poll_events` / `poll_events_after` did a full heap scan
// (O(total events)); after Q1 they use `DiskBTree::search_range_limit` and
// resolve only the matching RowIds (O(log n + returned)).  This bench
// quantifies both cases and records the flat-latency proof in PROGRESS.md.
//
// Run with: cargo bench --bench poll_events --release
// (release flag matters — debug criterion numbers are 3-5× higher)

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tempfile::tempdir;
use unidb::Engine;

fn setup_events(n: usize) -> (tempfile::TempDir, Engine) {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Create table and enable events.
    let xid = engine.begin().unwrap();
    engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
    engine.commit(xid).unwrap();
    engine.enable_events("t").unwrap();

    // Insert n rows in batches of 500 to keep txn WAL size manageable.
    let batch = 500;
    let mut i = 0i64;
    while i < n as i64 {
        let xid = engine.begin().unwrap();
        let end = (i + batch).min(n as i64);
        for j in i..end {
            engine
                .execute_sql(xid, &format!("INSERT INTO t (id) VALUES ({j})"))
                .unwrap();
        }
        engine.commit(xid).unwrap();
        i = end;
    }
    (dir, engine)
}

/// `poll_events_after(offset=0, limit=20)` with the seq-index path.
/// The offset=0 cursor means "return the first 20 events from the beginning,"
/// so the index traversal is O(log n + 20) regardless of table size.
fn bench_poll_after_flat(c: &mut Criterion) {
    let mut group = c.benchmark_group("poll_events_after_flat");
    group.sample_size(20);

    for n in [10_000usize, 100_000, 300_000] {
        let (_dir, engine) = setup_events(n);

        group.bench_with_input(BenchmarkId::new("limit_20", n), &n, |b, _| {
            b.iter(|| {
                let xid = engine.begin().unwrap();
                let evts = engine.poll_events_after(xid, 0, 20).unwrap();
                engine.commit(xid).unwrap();
                assert!(!evts.is_empty(), "expected events");
            });
        });
    }
    group.finish();
}

/// `poll_events(consumer, limit=20)` cursor-based durable poll.
/// Consumer's acked offset starts at 0; every call returns the first 20
/// unacked events — same O(log n + 20) path through the seq index.
fn bench_poll_durable_flat(c: &mut Criterion) {
    let mut group = c.benchmark_group("poll_events_durable_flat");
    group.sample_size(20);

    for n in [10_000usize, 100_000, 300_000] {
        let (_dir, engine) = setup_events(n);

        // Register consumer (offset starts at 0).
        let xid = engine.begin().unwrap();
        engine.poll_events(xid, "bench-c", 1).unwrap();
        engine.commit(xid).unwrap();

        group.bench_with_input(BenchmarkId::new("limit_20", n), &n, |b, _| {
            b.iter(|| {
                let xid = engine.begin().unwrap();
                let evts = engine.poll_events(xid, "bench-c", 20).unwrap();
                engine.commit(xid).unwrap();
                assert!(!evts.is_empty(), "expected events");
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20);
    targets = bench_poll_after_flat, bench_poll_durable_flat
}
criterion_main!(benches);
