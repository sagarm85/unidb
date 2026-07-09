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
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::tempdir;
use unidb::{AutoVacuumConfig, Engine};

/// The bloat a churn workload leaves behind: physical `data.db` bytes and the
/// logical page count (`stats().data_pages`). The page count is the honest
/// bloat metric — physical file size is quantized to the buffer pool's 4 MiB
/// mmap-grow chunks (P1.c), so at moderate scale it stays at one chunk for
/// every configuration and can't discriminate; the logical page count grows
/// one-for-one with un-reclaimed dead versions.
struct Bloat {
    bytes: u64,
    pages: u32,
}

/// Churn `keys` rows through `rounds` of full-table UPDATEs. When `vacuum_each`
/// is set, run `Engine::vacuum()` after every round so freed slots get reused.
fn churn_file_size(keys: u64, rounds: u64, vacuum_each: bool) -> Bloat {
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
    let pages = engine.stats().data_pages;
    drop(engine);
    Bloat {
        bytes: fs::metadata(dir.path().join("data.db")).unwrap().len(),
        pages,
    }
}

/// Churn `keys` rows through `rounds` of full-table UPDATEs with the background
/// **autovacuum launcher** running and **no manual `vacuum()` call anywhere** —
/// the A3/A4 proof that autovacuum bounds bloat on its own.
fn churn_file_size_autovacuum(keys: u64, rounds: u64) -> Bloat {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    // Aggressive-but-honest policy so the launcher keeps up with the churn.
    engine.set_autovacuum_config(AutoVacuumConfig {
        enabled: true,
        threshold: keys, // ~one round of dead versions
        scale_factor: 0.0,
        naptime: Duration::from_millis(20),
    });
    let engine = Arc::new(engine);
    engine.spawn_autovacuum();

    let mut rids = Vec::new();
    let x = engine.begin().unwrap();
    for i in 0..keys {
        rids.push(engine.insert(x, &i.to_le_bytes()).unwrap());
    }
    engine.commit(x).unwrap();
    for r in 0..rounds {
        let x = engine.begin().unwrap();
        for rid in rids.iter_mut() {
            // vacuum only reclaims superseded versions, never the live tip, so
            // `update`'s returned RowId stays valid alongside concurrent vacuum.
            *rid = engine.update(x, *rid, &[r as u8; 64]).unwrap();
        }
        engine.commit(x).unwrap();
    }
    // Let the launcher drain the last rounds' backlog (bounded wait).
    let deadline = Instant::now() + Duration::from_secs(10);
    while engine.dead_tuple_estimate() > keys && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    engine.flush().unwrap();
    engine.checkpoint().unwrap();
    let pages = engine.stats().data_pages;
    let bytes = fs::metadata(dir.path().join("data.db")).unwrap().len();
    drop(engine); // stops the launcher cleanly
    Bloat { bytes, pages }
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
        let bounded_auto = churn_file_size_autovacuum(keys, rounds);
        let (_dir, engine) = churned_engine(keys, rounds);
        let t0 = std::time::Instant::now();
        let report = engine.vacuum().unwrap();
        let elapsed = t0.elapsed();
        eprintln!(
            "[vacuum bench] keys={keys} rounds={rounds}: heap pages (logical bloat) \
             WITHOUT vacuum = {} ({} bytes), WITH periodic manual vacuum = {} ({:.1}x fewer pages), \
             WITH background AUTOVACUUM (no manual call) = {} ({:.1}x fewer pages); \
             single vacuum() over {} dead versions took {:?} ({} bytes reclaimed in-page)",
            grows.pages,
            grows.bytes,
            bounded.pages,
            grows.pages as f64 / bounded.pages.max(1) as f64,
            bounded_auto.pages,
            grows.pages as f64 / bounded_auto.pages.max(1) as f64,
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
