// P1.a benchmark: full-page-writes (WAL_FPI) cost.
//
// Measures the two things the P1.a spec asks to record: the WAL-size overhead
// from logging a full 8 KiB page image on the first modification of each page
// per checkpoint interval, and the insert throughput with FPI on. Not a
// criterion bench (we want file-size accounting, not just wall-clock), so it
// runs with `harness = false` and prints a table.
//
// Run with: cargo bench --bench fpi

use std::time::Instant;

use tempfile::tempdir;
use unidb::format::WAL_FPI;
use unidb::wal::Wal;
use unidb::Engine;

/// Insert `n` rows of `payload_len` bytes, one autocommit txn each, no manual
/// checkpoint (so every page pays exactly one FPI — its first touch). Returns
/// (elapsed_secs, wal_total_bytes, fpi_bytes, fpi_count, data_file_bytes).
fn run(n: u64, payload_len: usize) -> (f64, u64, u64, usize, u64) {
    let dir = tempdir().unwrap();
    let payload = vec![0xABu8; payload_len];

    let start = Instant::now();
    {
        let engine = Engine::open(dir.path(), 0).unwrap();
        for _ in 0..n {
            let xid = engine.begin().unwrap();
            engine.insert(xid, &payload).unwrap();
            engine.commit(xid).unwrap();
        }
        engine.flush().unwrap();
    }
    let elapsed = start.elapsed().as_secs_f64();

    // Account the WAL: total framed bytes and the share that is full-page images.
    let wal_path = dir.path().join("db.wal");
    let records = Wal::scan_file(&wal_path).unwrap();
    let mut fpi_bytes = 0u64;
    let mut fpi_count = 0usize;
    for r in &records {
        // Framed size ≈ 4 (len prefix) + 41 (fixed hdr) + redo + undo + 4 (crc).
        let framed = 4 + 41 + r.redo.len() + r.undo.len() + 4;
        if r.rec_type == WAL_FPI {
            fpi_bytes += framed as u64;
            fpi_count += 1;
        }
    }
    let wal_total = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    let data_bytes = std::fs::metadata(dir.path().join("data.db"))
        .map(|m| m.len())
        .unwrap_or(0);
    (elapsed, wal_total, fpi_bytes, fpi_count, data_bytes)
}

fn main() {
    println!("P1.a full-page-writes (WAL_FPI) — no manual checkpoint (one FPI per page)\n");
    println!(
        "{:>8}  {:>6}  {:>10}  {:>6}  {:>10}  {:>9}  {:>8}  {:>10}",
        "rows", "paylen", "wal_bytes", "#fpi", "fpi_bytes", "fpi_%wal", "ins/s", "data_bytes"
    );
    for &(n, len) in &[(2_000u64, 8usize), (2_000, 64), (2_000, 256), (2_000, 1024)] {
        let (secs, wal_total, fpi_bytes, fpi_count, data_bytes) = run(n, len);
        let pct = if wal_total > 0 {
            100.0 * fpi_bytes as f64 / wal_total as f64
        } else {
            0.0
        };
        let ips = n as f64 / secs;
        println!(
            "{:>8}  {:>6}  {:>10}  {:>6}  {:>10}  {:>8.2}%  {:>8.0}  {:>10}",
            n, len, wal_total, fpi_count, fpi_bytes, pct, ips, data_bytes
        );
    }
    println!(
        "\nfpi_%wal is the WAL-growth overhead from torn-page protection: one \n\
         8 KiB image per page on its first touch of the interval. It FALLS as \n\
         more rows share a page (small rows) and RISES as rows approach page \n\
         size. This insert-only, no-checkpoint run is close to the worst case \n\
         — every page is written once, so the fixed 8 KiB image is amortized \n\
         over only the few rows that fit in it. Under updates (many writes to \n\
         one page per interval) the same single image is amortized over far \n\
         more records, so the % drops sharply. Throughput is unchanged vs. \n\
         pre-FPI: the write path is fsync-bound (two fsyncs per autocommit \n\
         row), and an FPI adds WAL bytes but no extra fsync. Auto-checkpoint \n\
         (P1.e) bounds the total FPI volume: one image per page per interval."
    );
}
