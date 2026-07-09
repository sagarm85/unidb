// P5.e-3 headline benchmark: write throughput scales with concurrent writers.
//
// Before Phase 5, every write serialized through one dedicated writer thread
// (M5), so throughput was flat regardless of core count. Now `Engine` is
// `Send + Sync`, N threads share one `Arc<Engine>`, and group commit
// (`Wal::sync_up_to`) coalesces their commit fsyncs — so durable commit
// throughput should rise as writers are added, up to the machine's cores /
// fsync-batching limit.
//
// Each thread runs its own transactions: begin → insert one row → commit
// (durable). We report committed transactions/sec at 1/2/4/8 writer threads and
// the speedup vs a single writer. Deferred-sync (group-commit) mode is on, as
// in the server.
//
// Run: cargo bench --bench concurrent_writers   (or `cargo run --release
// --bin ...` style; harness = false, so this is a plain `main`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

use tempfile::tempdir;
use unidb::Engine;

/// Run `writers` threads, each committing `per_thread` single-row insert
/// transactions against the shared engine. Returns committed txns/sec.
fn run(writers: usize, per_thread: usize) -> f64 {
    let dir = tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    // Group-commit mode (the server default): per-statement fsyncs are deferred
    // and durability is forced (and coalesced) at commit.
    engine.set_deferred_sync(true);

    let barrier = Arc::new(Barrier::new(writers + 1));
    let committed = Arc::new(AtomicUsize::new(0));
    let payload = [0xABu8; 64];

    let mut handles = Vec::new();
    for _ in 0..writers {
        let engine = Arc::clone(&engine);
        let barrier = Arc::clone(&barrier);
        let committed = Arc::clone(&committed);
        handles.push(thread::spawn(move || {
            barrier.wait(); // start together
            for _ in 0..per_thread {
                let xid = engine.begin().unwrap();
                engine.insert(xid, &payload).unwrap();
                engine.commit(xid).unwrap();
            }
            committed.fetch_add(per_thread, Ordering::Relaxed);
        }));
    }

    barrier.wait();
    let start = Instant::now();
    for h in handles {
        h.join().unwrap();
    }
    let secs = start.elapsed().as_secs_f64();
    committed.load(Ordering::Relaxed) as f64 / secs
}

fn main() {
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    println!("concurrent write throughput (group-commit, {cores} logical cores)\n");
    println!("{:>8}  {:>16}  {:>10}", "writers", "commits/sec", "speedup");

    // Fixed total work per level (scaled so even 1 thread finishes quickly).
    let total = 20_000usize;
    let mut baseline = 0.0;
    for &writers in &[1usize, 2, 4, 8] {
        let per_thread = total / writers;
        let ops = run(writers, per_thread);
        if writers == 1 {
            baseline = ops;
        }
        println!("{:>8}  {:>16.0}  {:>9.2}x", writers, ops, ops / baseline);
    }
}
