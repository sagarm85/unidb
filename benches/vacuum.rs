// Vacuum benchmark (M10.d): the number that proves the space leak is closed.
//
// M1 UPDATE inserts a new tuple version and leaves the old one dead, so an
// update-heavy workload piles up one dead version per UPDATE. v1 vacuum does
// NOT lower the data file's high-water mark (that's a `VACUUM FULL`-class
// operation, backlog) — it makes the freed intra-page slots reusable. So the
// honest leak-closed proof is *not* "file shrinks after one vacuum"; it is:
//
//   churn WITHOUT interleaved vacuum   -> heap file grows unboundedly
//   churn WITH periodic vacuum         -> heap file stays bounded (slots reused)
//
// The one-shot block below reports both final file sizes; criterion then times
// the vacuum() call itself on a churned database.
//
// Run with: cargo bench --bench vacuum

use std::fs;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::tempdir;
use unidb::Engine;

/// Churn `keys` rows through `rounds` of full-table UPDATEs. When `vacuum_each`
/// is set, run `Engine::vacuum()` after every round so freed slots get reused.
/// Returns the resulting `data.db` size in bytes.
fn churn_file_size(keys: u64, rounds: u64, vacuum_each: bool) -> u64 {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let mut rids = Vec::new();
    let x = engine.begin().unwrap();
    for i in 0..keys {
        rids.push(engine.insert(x, &i.to_le_bytes()).unwrap());
    }
    engine.commit(x).unwrap();
    for r in 0..rounds {
        let x = engine.begin().unwrap();
        for rid in rids.iter_mut() {
            *rid = engine.update(x, *rid, &[r as u8; 64]).unwrap();
        }
        engine.commit(x).unwrap();
        if vacuum_each {
            // Re-resolve current RowIds after compaction may relocate/reuse.
            let report = engine.vacuum().unwrap();
            let _ = report;
            // Rebuild rid handles from a fresh scan is unnecessary here: update
            // returns the new RowId each round, and vacuum only reclaims the
            // *superseded* versions, never the live tip, so `rids` stays valid.
        }
    }
    engine.flush().unwrap();
    engine.checkpoint().unwrap();
    drop(engine);
    fs::metadata(dir.path().join("data.db")).unwrap().len()
}

/// Build a churned DB (no interleaved vacuum) and return it with its live
/// RowIds, so the caller can time a single `vacuum()` over the full backlog.
fn churned_engine(keys: u64, rounds: u64) -> (tempfile::TempDir, Engine) {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let mut rids = Vec::new();
    let x = engine.begin().unwrap();
    for i in 0..keys {
        rids.push(engine.insert(x, &i.to_le_bytes()).unwrap());
    }
    engine.commit(x).unwrap();
    for r in 0..rounds {
        let x = engine.begin().unwrap();
        for rid in rids.iter_mut() {
            *rid = engine.update(x, *rid, &[r as u8; 64]).unwrap();
        }
        engine.commit(x).unwrap();
    }
    (dir, engine)
}

fn bench_vacuum(c: &mut Criterion) {
    // One-shot report (stderr) at the honest, realistic scale. The churn setup
    // is per-statement-fsync-bound (~ms/op), so criterion's 100-sample loop
    // over it would take hours — instead we time a single `vacuum()` over the
    // full backlog directly, and let criterion run only the small-scale timing.
    {
        let keys = 200u64;
        let rounds = 30u64;
        let grows = churn_file_size(keys, rounds, false);
        let bounded = churn_file_size(keys, rounds, true);
        let (_dir, engine) = churned_engine(keys, rounds);
        let t0 = std::time::Instant::now();
        let report = engine.vacuum().unwrap();
        let elapsed = t0.elapsed();
        eprintln!(
            "[vacuum bench] keys={keys} rounds={rounds}: heap file WITHOUT vacuum = {grows} bytes, \
             WITH periodic vacuum = {bounded} bytes ({:.1}x smaller); single vacuum() over \
             {} dead versions took {:?} ({} bytes reclaimed in-page)",
            grows as f64 / bounded.max(1) as f64,
            report.versions_reclaimed,
            elapsed,
            report.bytes_reclaimed,
        );
    }

    // Small-scale criterion timing so `cargo bench` still produces a stable
    // number without a multi-hour run.
    let (keys, rounds) = (50u64, 8u64);
    let mut group = c.benchmark_group("vacuum");
    group.sample_size(10);
    group.throughput(Throughput::Elements(keys * rounds));
    group.bench_with_input(
        BenchmarkId::from_parameter(format!("{keys}x{rounds}")),
        &(keys, rounds),
        |b, &(keys, rounds)| {
            b.iter_batched(
                || churned_engine(keys, rounds),
                |(dir, engine)| {
                    engine.vacuum().unwrap();
                    (dir, engine)
                },
                criterion::BatchSize::SmallInput,
            );
        },
    );
    group.finish();
}

criterion_group!(benches, bench_vacuum);
criterion_main!(benches);
