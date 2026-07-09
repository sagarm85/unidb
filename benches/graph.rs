// Graph benchmarks (M3.b/M3.d). `adjacency_scan` measures the batch-latch
// optimization's actual effect on a hot-hub workload — CLAUDE.md §6 wants
// this reported honestly, not assumed, since edge rows are small enough
// that page fill factor could bound the win below naive intuition.
// Run with: cargo bench --bench graph

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tempfile::tempdir;
use unidb::bufferpool::BufferPool;
use unidb::csr_index::CsrIndex;
use unidb::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
use unidb::graph::edges::{edge_row, edges_table_def};
use unidb::graph::index::resolve_candidates_batched;
use unidb::heap::{Heap, RowId};
use unidb::mvcc::Snapshot;
use unidb::sql::executor::encode_row;
use unidb::wal::Wal;

/// Populate a heap with `n` edge rows (all conceptually from the same hot
/// `from_id` — `Heap` doesn't care about column content, only bytes) and
/// return every `RowId`, a ready `BufferPool`, and a snapshot that sees
/// them all.
fn build_hot_hub(dir: &std::path::Path, n: u64) -> (Heap, BufferPool, Vec<RowId>, Snapshot) {
    let pool = BufferPool::open(&dir.join("data.db"), DEFAULT_PAGE_SIZE as usize, 256).unwrap();
    let wal = Wal::open(&dir.join("db.wal"), INVALID_LSN).unwrap();
    let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);

    let mut ids = Vec::with_capacity(n as usize);
    for i in 0..n {
        let encoded = encode_row(&edge_row(1, i as i64, "KNOWS", "{}"));
        let rid = heap.insert(&encoded, 1, &pool, &wal).unwrap();
        ids.push(rid);
    }
    let snapshot = Snapshot::new(2, 2, vec![]);
    (heap, pool, ids, snapshot)
}

/// One `Heap::get` per candidate — the naive resolution `edges_from` used
/// before the batch-latch optimization. Kept only in this benchmark for
/// comparison; the shipped path is `resolve_candidates_batched`.
fn resolve_naive(
    heap: &Heap,
    candidates: &[RowId],
    snapshot: &Snapshot,
    pool: &BufferPool,
) -> usize {
    let mut found = 0;
    for &rid in candidates {
        if heap.get(rid, snapshot, 2, pool).is_ok() {
            found += 1;
        }
    }
    found
}

fn bench_adjacency_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("adjacency_scan");
    let columns = edges_table_def().columns;

    for n in [1_000u64, 10_000] {
        let dir = tempdir().unwrap();
        let (heap, pool, ids, snapshot) = build_hot_hub(dir.path(), n);
        let distinct_pages: std::collections::HashSet<_> = ids.iter().map(|r| r.page_id).collect();
        eprintln!(
            "adjacency_scan n={n}: {} distinct pages",
            distinct_pages.len()
        );

        group.bench_with_input(BenchmarkId::new("naive", n), &n, |b, _| {
            b.iter(|| resolve_naive(&heap, &ids, &snapshot, &pool));
        });

        group.bench_with_input(BenchmarkId::new("batched", n), &n, |b, _| {
            b.iter(|| resolve_candidates_batched(&ids, &snapshot, 2, &pool, &columns).unwrap());
        });

        // M7: CSR-backed candidate fetch + the same batched resolve/
        // revalidate step the other two variants pay — isolates whether
        // CSR's binary-search candidate lookup adds/removes anything
        // measurable over EdgeIndex's O(1) HashMap lookup for this
        // single-hop workload (expected: no meaningful difference, since
        // the batched resolve step dominates either way — CSR's real
        // value is future multi-hop traversal, not this shape).
        let mut csr = CsrIndex::new();
        for &id in &ids {
            csr.stage(1, id);
        }
        csr.rebuild();
        group.bench_with_input(BenchmarkId::new("csr", n), &n, |b, _| {
            b.iter(|| {
                let candidates = csr.candidates(1).to_vec();
                resolve_candidates_batched(&candidates, &snapshot, 2, &pool, &columns).unwrap()
            });
        });
    }
    group.finish();
}

fn bench_edge_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("edge_insert");
    group.bench_function("uncontended", |b| {
        b.iter(|| {
            let dir = tempdir().unwrap();
            let engine = unidb::Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            for i in 0..100u64 {
                engine.create_edge(xid, 1, i as i64, "KNOWS", "{}").unwrap();
            }
            engine.commit(xid).unwrap();
        });
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_adjacency_scan, bench_edge_insert
}
criterion_main!(benches);
