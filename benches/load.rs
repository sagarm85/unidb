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
                    engine.insert(&payload).unwrap();
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
    for i in 0u64..1_000 {
        rids.push(engine.insert(&i.to_le_bytes()).unwrap());
    }
    engine.flush().unwrap();

    group.throughput(Throughput::Elements(1));
    group.bench_function("point_get", |b| {
        let mut idx = 0usize;
        b.iter(|| {
            let rid = rids[idx % rids.len()];
            let _ = engine.get(rid).unwrap();
            idx += 1;
        });
    });
    group.finish();
}

fn bench_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("update");
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    // Pre-insert with 16-byte payloads so update fits in-place.
    let rids: Vec<_> = (0u64..1_000)
        .map(|i| {
            let mut buf = [0u8; 16];
            buf[..8].copy_from_slice(&i.to_le_bytes());
            engine.insert(&buf).unwrap()
        })
        .collect();

    group.throughput(Throughput::Elements(1));
    group.bench_function("update_in_place", |b| {
        let mut idx = 0usize;
        let new_val = [0xffu8; 8];
        b.iter(|| {
            let rid = rids[idx % rids.len()];
            engine.update(rid, &new_val).unwrap();
            idx += 1;
        });
    });
    group.finish();
}

criterion_group!(benches, bench_insert, bench_select, bench_update);
criterion_main!(benches);
