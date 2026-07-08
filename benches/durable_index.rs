// P3.a benchmark: the Phase-3 gate is that `Engine::open` is O(1) regardless of
// data size — a durable B-Tree is read from its meta page, never rebuilt by
// rescanning the heap. The number that proves it: reopen time as a table's
// indexed-row count grows.
//
//   (A) A BTree-indexed table: reopen time must stay ~flat as rows grow —
//       there is no heap rescan on open anymore (P3.a removed BTree from
//       `rebuild_secondary_indexes`).
//
//   (B) The contrast: an HNSW-indexed table of the same size. That index is
//       still in-memory and rebuilt on open, so `Engine::open` synchronously
//       rescans the whole heap to re-enqueue every row — reopen time grows with
//       the row count. This is exactly the O(data) startup Phase 3 kills, shown
//       side by side with the durable path that fixed it.
//
// Absolute numbers are machine-dependent; the number to watch is the *scaling*:
// column (A) flat, column (B) rising with row count.
//
// Run with: cargo bench --bench durable_index

use std::time::Instant;

use tempfile::tempdir;
use unidb::Engine;

/// Build a table `t` with `n` rows and an index of the given kind on a column,
/// using batched multi-row INSERTs so setup stays as fast as the per-row fsync
/// floor allows (one user-txn commit per batch). Returns nothing — the data
/// lives in `dir` for a subsequent reopen to be timed.
fn build(dir: &std::path::Path, n: u64, hnsw: bool) {
    let mut engine = Engine::open(dir, 0).unwrap();
    let xid = engine.begin().unwrap();
    if hnsw {
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
            .unwrap();
    } else {
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING BTREE (id)")
            .unwrap();
    }
    engine.commit(xid).unwrap();

    const BATCH: u64 = 500;
    let mut i = 0;
    while i < n {
        let end = (i + BATCH).min(n);
        let values: Vec<String> = (i..end)
            .map(|j| {
                if hnsw {
                    format!("({j}, [{j}.0, {j}.0])")
                } else {
                    format!("({j}, 'row{j}')")
                }
            })
            .collect();
        let cols = if hnsw { "id, embedding" } else { "id, name" };
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(
                xid,
                &format!("INSERT INTO t ({cols}) VALUES {}", values.join(", ")),
            )
            .unwrap();
        engine.commit(xid).unwrap();
        i = end;
    }
    // Checkpoint so the reopen we time exercises the real steady-state open
    // path (control file + WAL replay + index bring-up), not a giant WAL.
    engine.checkpoint().unwrap();
    drop(engine);
}

/// Median reopen (`Engine::open`) time over a few trials, in milliseconds.
fn median_reopen_ms(dir: &std::path::Path) -> f64 {
    let mut samples = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let engine = Engine::open(dir, 0).unwrap();
        let ms = start.elapsed().as_secs_f64() * 1e3;
        drop(engine);
        samples.push(ms);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

fn main() {
    println!("P3.a — Engine::open cost vs. indexed-row count\n");
    println!(
        "{:>8}  {:>18}  {:>18}",
        "rows", "BTree open (ms)", "HNSW open (ms)"
    );
    println!(
        "{:>8}  {:>18}  {:>18}",
        "", "(durable, P3.a)", "(rebuilt on open)"
    );

    for &n in &[1_000u64, 3_000, 6_000] {
        let btree_dir = tempdir().unwrap();
        build(btree_dir.path(), n, false);
        let btree_ms = median_reopen_ms(btree_dir.path());

        let hnsw_dir = tempdir().unwrap();
        build(hnsw_dir.path(), n, true);
        let hnsw_ms = median_reopen_ms(hnsw_dir.path());

        println!("{n:>8}  {btree_ms:>18.3}  {hnsw_ms:>18.3}");
    }

    println!(
        "\nRead the *scaling*: the durable B-Tree column stays flat (O(1) open,\n\
         no heap rescan); the rebuilt-on-open HNSW column rises with row count."
    );
}
