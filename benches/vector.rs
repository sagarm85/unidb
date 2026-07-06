// Vector/full-text load benchmark (M2.d). Baseline comparison: Postgres +
// pgvector, an interim proxy for "the replaced stack" per CLAUDE.md §6 (the
// full four-system cross-domain comparison is deferred to M4, since graph
// and queue don't exist yet — see PROGRESS.md's M2 entry).
// Run with: cargo bench --bench vector

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::tempdir;
use unidb::fulltext::InvertedIndex;
use unidb::heap::RowId;
use unidb::index_worker::IndexStatus;
use unidb::vector::VectorIndex;
use unidb::Engine;

const DIM: usize = 128;

fn embedding_literal(seed: u64) -> String {
    let vals: Vec<String> = (0..DIM)
        .map(|i| format!("{:.4}", ((seed as f64) * 0.001 + i as f64).sin()))
        .collect();
    format!("[{}]", vals.join(", "))
}

fn wait_ready(engine: &Engine, table: &str, column: &str) {
    let start = std::time::Instant::now();
    while engine.index_status(table, column) != Some(IndexStatus::Ready) {
        if start.elapsed() > std::time::Duration::from_secs(30) {
            panic!("index never reached Ready");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// INSERT throughput into a `VECTOR(128)` column, with vs without an active
/// HNSW index — the point is to check CLAUDE.md's M2 claim that "row write
/// is the only synchronous cost" actually holds (the index rebuild happens
/// off-thread in the background worker, not on this INSERT's critical
/// path). Modest row counts (`VectorIndex` rebuilds its whole graph per
/// upsert — see MEMORY.md's M2.b design note — so this isn't testing HNSW
/// build cost at scale, just foreground overhead).
fn bench_vector_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector_insert");
    for rows in [50u64, 200] {
        group.throughput(Throughput::Elements(rows));
        group.bench_with_input(BenchmarkId::new("without_index", rows), &rows, |b, &n| {
            b.iter(|| {
                let dir = tempdir().unwrap();
                let mut engine = Engine::open(dir.path(), 0).unwrap();
                let xid = engine.begin().unwrap();
                engine
                    .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(128))")
                    .unwrap();
                for i in 0..n {
                    let sql = format!(
                        "INSERT INTO t (id, embedding) VALUES ({i}, {})",
                        embedding_literal(i)
                    );
                    engine.execute_sql(xid, &sql).unwrap();
                }
                engine.commit(xid).unwrap();
            });
        });
        group.bench_with_input(BenchmarkId::new("with_index", rows), &rows, |b, &n| {
            b.iter(|| {
                let dir = tempdir().unwrap();
                let mut engine = Engine::open(dir.path(), 0).unwrap();
                let xid = engine.begin().unwrap();
                engine
                    .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(128))")
                    .unwrap();
                engine
                    .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
                    .unwrap();
                for i in 0..n {
                    let sql = format!(
                        "INSERT INTO t (id, embedding) VALUES ({i}, {})",
                        embedding_literal(i)
                    );
                    engine.execute_sql(xid, &sql).unwrap();
                }
                engine.commit(xid).unwrap();
            });
        });
    }
    group.finish();
}

/// `NEAR` query latency at a few `k` values, against a pre-built, `Ready`
/// index of 300 rows.
fn bench_near_query(c: &mut Criterion) {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(128))")
        .unwrap();
    engine
        .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
        .unwrap();
    for i in 0..300u64 {
        let sql = format!(
            "INSERT INTO t (id, embedding) VALUES ({i}, {})",
            embedding_literal(i)
        );
        engine.execute_sql(xid, &sql).unwrap();
    }
    engine.commit(xid).unwrap();
    wait_ready(&engine, "t", "embedding");

    let mut group = c.benchmark_group("near_query");
    for k in [5u64, 20, 50] {
        group.bench_with_input(BenchmarkId::from_parameter(k), &k, |b, &k| {
            b.iter(|| {
                let xid = engine.begin().unwrap();
                let sql = format!(
                    "SELECT id FROM t WHERE NEAR(embedding, {}, {k})",
                    embedding_literal(0)
                );
                let _ = engine.execute_sql(xid, &sql).unwrap();
                engine.commit(xid).unwrap();
            });
        });
    }
    group.finish();
}

/// Raw `VectorIndex`/`InvertedIndex` primitives, isolated from SQL/engine
/// overhead — there is no SQL-level full-text query surface in M2 (only
/// `NEAR` for vectors; see MEMORY.md), so this is the only way to
/// characterize full-text index cost this milestone.
fn bench_index_primitives(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_primitives");

    group.bench_function("vector_index_upsert_100", |b| {
        b.iter(|| {
            let mut idx = VectorIndex::new();
            for i in 0..100u32 {
                idx.upsert(
                    RowId {
                        page_id: 0,
                        slot: i as u16,
                    },
                    vec![i as f32; DIM],
                );
            }
        });
    });

    let mut fulltext_idx = InvertedIndex::new();
    for i in 0..300u32 {
        fulltext_idx.upsert(
            RowId {
                page_id: 0,
                slot: i as u16,
            },
            &format!("row number {i} contains some sample english words about databases"),
        );
    }
    group.bench_function("fulltext_search", |b| {
        b.iter(|| {
            let _ = fulltext_idx.search("sample databases");
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_vector_insert,
    bench_near_query,
    bench_index_primitives
);
criterion_main!(benches);
