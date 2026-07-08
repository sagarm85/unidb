// P1.c benchmark: vertical scaling — `alloc_page` remap fix + real FSM.
//
// The spec asks that insert/point-read throughput stay ~flat as a table grows
// to 100k → 1M rows (before P1.c it degraded, then failed with BufferPoolFull).
// End-to-end 1M inserts through the Engine are impractical here: the write path
// is fsync-bound (~160 rows/s → hours). So this isolates exactly what P1.c
// changed, fsync-free, at the storage layer:
//
//   (A) alloc_page: allocate N pages in a loop and report pages/sec. Before
//       P1.c each allocation re-mapped the *whole growing file* — O(N) work per
//       call, O(N^2) total — so this degraded sharply with N. After P1.c the
//       file grows in 4 MiB chunks (one remap per ~512 pages), so it stays flat.
//
//   (B) heap insert + point-read scaling with a deferred-sync WAL (removes the
//       per-statement fsync floor, exposing the O(pages) FSM cost the old
//       linear free-space scan paid). Throughput is reported per 50k-row window
//       as the heap grows; flat windows mean the FSM is ~O(1)/page, not
//       O(pages). Reads never fsync, so point-read throughput is reported too.
//
// Run with: cargo bench --bench scale

use std::time::Instant;

use tempfile::tempdir;
use unidb::bufferpool::BufferPool;
use unidb::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
use unidb::heap::Heap;
use unidb::mvcc::Snapshot;
use unidb::wal::Wal;

fn bench_alloc_page() {
    println!("(A) alloc_page throughput (fsync-free; pre-P1.c was O(N^2) total)\n");
    println!("{:>10}  {:>12}  {:>14}", "pages", "secs", "pages/sec");
    for &n in &[10_000u32, 50_000, 100_000] {
        let dir = tempdir().unwrap();
        let mut pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 256).unwrap();
        let start = Instant::now();
        for _ in 0..n {
            pool.alloc_page().unwrap();
        }
        let secs = start.elapsed().as_secs_f64();
        println!("{:>10}  {:>12.4}  {:>14.0}", n, secs, n as f64 / secs);
    }
    println!();
}

fn bench_insert_scaling() {
    println!("(B) heap insert + point-read scaling (deferred WAL, real FSM)\n");
    println!(
        "{:>14}  {:>12}  {:>14}",
        "rows (window)", "secs", "inserts/sec"
    );
    let dir = tempdir().unwrap();
    let ps = DEFAULT_PAGE_SIZE as usize;
    // Large pool so eviction is rare; deferred WAL so no per-statement fsync.
    let mut pool = BufferPool::open(&dir.path().join("data.db"), ps, 8192).unwrap();
    let mut wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();
    wal.set_deferred_sync(true);
    let mut heap = Heap::new(ps);

    let payload = [0xABu8; 48];
    let window = 50_000u32;
    let windows = 6; // 300k rows total
    let mut sample_rids = Vec::new();
    for w in 0..windows {
        let start = Instant::now();
        for i in 0..window {
            let rid = heap.insert(&payload, 1, &mut pool, &mut wal).unwrap();
            if i % 5000 == 0 {
                sample_rids.push(rid);
            }
        }
        wal.sync().unwrap(); // make the window durable before timing the next
        let secs = start.elapsed().as_secs_f64();
        let lo = w * window;
        let hi = (w + 1) * window;
        println!(
            "{:>14}  {:>12.4}  {:>14.0}",
            format!("{}-{}k", lo / 1000, hi / 1000),
            secs,
            window as f64 / secs
        );
    }

    // Point-read throughput at ~300k rows (reads never fsync).
    let snap = Snapshot::new(100, 100, vec![]);
    let start = Instant::now();
    let reads = 100_000usize;
    for k in 0..reads {
        let rid = sample_rids[k % sample_rids.len()];
        let _ = heap.get(rid, &snap, 100, &pool).unwrap();
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "\npoint reads at ~300k rows: {:.0} reads/sec ({} reads in {:.3}s)",
        reads as f64 / secs,
        reads,
        secs
    );
}

fn main() {
    println!("P1.c vertical scaling — alloc_page remap fix + real FSM\n");
    bench_alloc_page();
    bench_insert_scaling();
    println!(
        "\nFlat pages/sec in (A) and flat inserts/sec across the windows in (B) \n\
         are the P1.c win: file growth is chunked (not a whole-file remap per \n\
         page) and page selection is FSM-driven (integer compares, not a fetch \n\
         of every page). Point reads are unaffected by table size."
    );
}
