// B-Tree secondary index benchmark (M6). The number that demonstrates the
// win: point/range SELECT cost on an indexed column vs. an unindexed
// (full-scan) column, at increasing row counts. Both paths still pay the
// same per-statement transaction/fsync overhead documented since M1 (a
// read-only statement's `commit()` unconditionally fsyncs — known tech
// debt, not fixed here) — the number to watch is the *scaling* difference
// between the two (full-scan grows with row count, index-assisted should
// stay roughly flat), not the absolute latency.
// Run with: cargo bench --bench btree

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tempfile::tempdir;
use unidb::catalog::IndexStatus;
use unidb::Engine;

/// Set up one table with `n` rows and (optionally) a BTree index on `id`,
/// using batched multi-row `INSERT` statements so setup itself stays fast
/// regardless of `n` — this only affects one-time setup cost, not anything
/// being measured.
fn setup_table(engine: &mut Engine, table: &str, n: u64, indexed: bool) {
    let ddl_xid = engine.begin().unwrap();
    engine
        .execute_sql(
            ddl_xid,
            &format!("CREATE TABLE {table} (id INT, name TEXT)"),
        )
        .unwrap();
    if indexed {
        engine
            .execute_sql(
                ddl_xid,
                &format!("CREATE INDEX idx_{table} ON {table} USING BTREE (id)"),
            )
            .unwrap();
    }
    engine.commit(ddl_xid).unwrap();

    // One commit per batch, not one giant transaction for all `n` rows —
    // the fixed-size (256-frame) buffer pool keeps every page a still-open
    // transaction has touched pinned until commit, so a single transaction
    // spanning tens of thousands of rows across many pages exhausts it
    // (`DbError::BufferPoolFull`) regardless of how the INSERTs are batched
    // within it. Committing periodically releases those pins.
    const BATCH: u64 = 500;
    let mut i = 0;
    while i < n {
        let end = (i + BATCH).min(n);
        let values: Vec<String> = (i..end).map(|j| format!("({j}, 'row{j}')")).collect();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(
                xid,
                &format!(
                    "INSERT INTO {table} (id, name) VALUES {}",
                    values.join(", ")
                ),
            )
            .unwrap();
        engine.commit(xid).unwrap();
        i = end;
    }

    if indexed {
        let start = std::time::Instant::now();
        loop {
            if engine.index_status(table, "id") == Some(IndexStatus::Ready) {
                break;
            }
            if start.elapsed() > std::time::Duration::from_secs(30) {
                panic!("index never reached Ready");
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

fn bench_point_select(c: &mut Criterion) {
    let mut group = c.benchmark_group("btree_point_select");
    // 100_000 deliberately excluded: two 100k-row tables in one engine hit
    // `DbError::BufferPoolFull` during setup even with per-batch commits —
    // a real, separately-tracked buffer-pool/FSM scaling limit (see
    // MEMORY.md's M6 known-issues entry), not a B-Tree-specific bug and out
    // of this benchmark's scope to fix.
    for n in [1_000u64, 10_000] {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        setup_table(&mut engine, "indexed", n, true);
        setup_table(&mut engine, "plain", n, false);
        let target = n / 2;

        group.bench_with_input(BenchmarkId::new("indexed", n), &n, |b, _| {
            b.iter(|| {
                let xid = engine.begin().unwrap();
                let _ = engine
                    .execute_sql(
                        xid,
                        &format!("SELECT name FROM indexed WHERE id = {target}"),
                    )
                    .unwrap();
                engine.commit(xid).unwrap();
            });
        });
        group.bench_with_input(BenchmarkId::new("full_scan", n), &n, |b, _| {
            b.iter(|| {
                let xid = engine.begin().unwrap();
                let _ = engine
                    .execute_sql(xid, &format!("SELECT name FROM plain WHERE id = {target}"))
                    .unwrap();
                engine.commit(xid).unwrap();
            });
        });
    }
    group.finish();
}

fn bench_range_select(c: &mut Criterion) {
    let mut group = c.benchmark_group("btree_range_select");
    // 100_000 deliberately excluded: two 100k-row tables in one engine hit
    // `DbError::BufferPoolFull` during setup even with per-batch commits —
    // a real, separately-tracked buffer-pool/FSM scaling limit (see
    // MEMORY.md's M6 known-issues entry), not a B-Tree-specific bug and out
    // of this benchmark's scope to fix.
    for n in [1_000u64, 10_000] {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        setup_table(&mut engine, "indexed", n, true);
        setup_table(&mut engine, "plain", n, false);
        // A narrow range near the top of the key space, well under 1% of
        // rows — the case an index should help with most.
        let lo = n.saturating_sub(10);

        group.bench_with_input(BenchmarkId::new("indexed", n), &n, |b, _| {
            b.iter(|| {
                let xid = engine.begin().unwrap();
                let _ = engine
                    .execute_sql(xid, &format!("SELECT name FROM indexed WHERE id > {lo}"))
                    .unwrap();
                engine.commit(xid).unwrap();
            });
        });
        group.bench_with_input(BenchmarkId::new("full_scan", n), &n, |b, _| {
            b.iter(|| {
                let xid = engine.begin().unwrap();
                let _ = engine
                    .execute_sql(xid, &format!("SELECT name FROM plain WHERE id > {lo}"))
                    .unwrap();
                engine.commit(xid).unwrap();
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_point_select, bench_range_select
}
criterion_main!(benches);
