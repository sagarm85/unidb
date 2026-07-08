// P1.e benchmark: auto-checkpoint bounds WAL growth.
//
// Before P1.e a checkpoint was manual-only, so the WAL (and the P1.a full-page-
// image volume it now carries) grew without bound for the life of a session.
// Auto-checkpoint fires the existing checkpoint path inline when the WAL crosses
// `max_wal_size` (or after `checkpoint_timeout`) at a quiescent point, truncating
// the WAL. This bench inserts the same workload with auto-checkpoint OFF vs ON
// and reports the final on-disk WAL size (bounded vs unbounded) and throughput
// (the cost of the extra checkpoint I/O).
//
// Run with: cargo bench --bench checkpoint

use std::time::{Duration, Instant};

use tempfile::tempdir;
use unidb::{AutoCheckpointConfig, Engine};

/// Insert `n` rows one-per-autocommit-txn; return (secs, final WAL file bytes,
/// auto-checkpoints fired).
fn run(n: u64, auto: Option<u64>) -> (f64, u64, u64) {
    let dir = tempdir().unwrap();
    let mut engine = Engine::open(dir.path(), 0).unwrap();
    engine.set_auto_checkpoint_config(AutoCheckpointConfig {
        enabled: auto.is_some(),
        timeout: Duration::from_secs(3600), // size-triggered only, deterministic
        max_wal_size: auto.unwrap_or(u64::MAX),
    });

    let payload = [0xABu8; 48];
    let start = Instant::now();
    for _ in 0..n {
        let xid = engine.begin().unwrap();
        engine.insert(xid, &payload).unwrap();
        engine.commit(xid).unwrap();
    }
    let secs = start.elapsed().as_secs_f64();
    let checkpoints = engine.checkpoints_triggered();
    drop(engine);
    let wal_bytes = std::fs::metadata(dir.path().join("db.wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    (secs, wal_bytes, checkpoints)
}

fn main() {
    println!("P1.e auto-checkpoint — bounded WAL growth\n");
    println!(
        "{:>18}  {:>6}  {:>12}  {:>11}  {:>8}",
        "config", "rows", "wal_bytes", "checkpoints", "rows/s"
    );

    let n = 3_000u64;
    let (secs, wal, ck) = run(n, None);
    println!(
        "{:>18}  {:>6}  {:>12}  {:>11}  {:>8.0}",
        "auto OFF",
        n,
        wal,
        ck,
        n as f64 / secs
    );

    for &limit in &[64 * 1024u64, 256 * 1024] {
        let (secs, wal, ck) = run(n, Some(limit));
        println!(
            "{:>18}  {:>6}  {:>12}  {:>11}  {:>8.0}",
            format!("auto {}KiB", limit / 1024),
            n,
            wal,
            ck,
            n as f64 / secs
        );
    }

    println!(
        "\nWith auto-checkpoint OFF the WAL grows with the whole workload; with it \n\
         ON the final WAL stays near the configured max_wal_size (the tail since \n\
         the last checkpoint) regardless of how many rows were written. Throughput \n\
         is close to the manual-checkpoint path — the write floor is still the \n\
         per-statement fsync; checkpoints add periodic flush I/O amortized across \n\
         many commits (one per ~max_wal_size of WAL)."
    );
}
