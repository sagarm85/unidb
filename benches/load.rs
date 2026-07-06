// Load benchmark: INSERT / point-SELECT / UPDATE throughput + latency.
// Baseline comparison: SQLite (see CLAUDE.md §6).
// Run with: cargo bench

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::tempdir;
use unidb::Engine;

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");
    for rows in [100u64, 1_000, 10_000] {
        group.throughput(Throughput::Elements(rows));
        group.bench_with_input(BenchmarkId::from_parameter(rows), &rows, |b, &n| {
            b.iter(|| {
                let dir = tempdir().unwrap();
                let mut engine = Engine::open(dir.path(), 0).unwrap();
                for i in 0..n {
                    let payload = i.to_le_bytes();
                    let xid = engine.begin().unwrap();
                    engine.insert(xid, &payload).unwrap();
                    engine.commit(xid).unwrap();
                }
                engine.flush().unwrap();
            });
        });
    }
    group.finish();
}

fn bench_select(c: &mut Criterion) {
    let mut group = c.benchmark_group("select_point");
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    let mut rids = Vec::new();
    let setup_xid = engine.begin().unwrap();
    for i in 0u64..1_000 {
        rids.push(engine.insert(setup_xid, &i.to_le_bytes()).unwrap());
    }
    engine.commit(setup_xid).unwrap();
    engine.flush().unwrap();

    group.throughput(Throughput::Elements(1));
    group.bench_function("point_get", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let rid = rids[idx % rids.len()];
            let xid = engine.begin().unwrap();
            let _ = engine.get(xid, rid).unwrap();
            engine.commit(xid).unwrap();
            idx += 1;
        });
    });
    group.finish();
}

fn bench_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("update");
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    let setup_xid = engine.begin().unwrap();
    let mut rids: Vec<_> = (0u64..1_000)
        .map(|i| {
            let mut buf = [0u8; 16];
            buf[..8].copy_from_slice(&i.to_le_bytes());
            engine.insert(setup_xid, &buf).unwrap()
        })
        .collect();
    engine.commit(setup_xid).unwrap();

    group.throughput(Throughput::Elements(1));
    group.bench_function("update_mvcc", |b| {
        let mut idx = 0usize;
        let new_val = [0xffu8; 8];
        b.iter(|| {
            // M1: UPDATE creates a new version rather than overwriting in
            // place, so each iteration must track the row's latest RowId.
            let i = idx % rids.len();
            let xid = engine.begin().unwrap();
            let new_rid = engine.update(xid, rids[i], &new_val).unwrap();
            engine.commit(xid).unwrap();
            rids[i] = new_rid;
            idx += 1;
        });
    });
    group.finish();
}

/// M1.d: transactional contention workload — measures the cost of SI's
/// abort-on-conflict + retry pattern (D12), not just uncontended CRUD. The
/// engine is single-threaded (sync-only per CLAUDE.md), so "contention"
/// here means interleaved sessions on one thread, not real parallelism:
/// xid_a and xid_b both hold open transactions; xid_a updates a row, xid_b's
/// attempt on the same row hits the lock and aborts immediately (no
/// blocking, per D12), then retries against the now-current version.
fn bench_contention(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention");
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    let setup_xid = engine.begin().unwrap();
    let mut rid = engine.insert(setup_xid, b"initial").unwrap();
    engine.commit(setup_xid).unwrap();

    group.throughput(Throughput::Elements(1));
    group.bench_function("update_conflict_abort_retry", |b| {
        b.iter(|| {
            let xid_a = engine.begin().unwrap();
            let xid_b = engine.begin().unwrap(); // concurrent session, already open
            let new_rid = engine.update(xid_a, rid, b"a-wins").unwrap();
            if engine.update(xid_b, rid, b"b-loses").is_err() {
                engine.abort(xid_b).unwrap();
            }
            engine.commit(xid_a).unwrap();

            let xid_b2 = engine.begin().unwrap();
            let final_rid = engine.update(xid_b2, new_rid, b"b-retry-wins").unwrap();
            engine.commit(xid_b2).unwrap();
            rid = final_rid;
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_insert,
    bench_select,
    bench_update,
    bench_contention
);
criterion_main!(benches);
