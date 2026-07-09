// Phase 6 ops benchmarks (§6 honest measurement): base backup, restore, PITR,
// replica apply throughput + lag, and failover time. Plain `main` (harness =
// false) — prints a table rather than using criterion, since these are
// coarse-grained ops timings, not micro-benchmarks.

use std::time::Instant;

use tempfile::tempdir;
use unidb::replication::Replica;
use unidb::Engine;

fn seed(engine: &Engine, rows: usize) {
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE t (id INT, v INT)")
        .unwrap();
    engine.commit(x).unwrap();
    // Batch inserts in transactions of 100 to keep it quick but realistic.
    let mut i = 0;
    while i < rows {
        let x = engine.begin().unwrap();
        for _ in 0..100.min(rows - i) {
            engine
                .execute_sql(x, &format!("INSERT INTO t (id, v) VALUES ({i}, {})", i * 2))
                .unwrap();
            i += 1;
        }
        engine.commit(x).unwrap();
    }
}

fn main() {
    let rows = 5_000usize;
    println!("Phase 6 ops benchmark — {rows} rows\n");

    // ── base backup + restore + PITR ────────────────────────────────────────
    let src = tempdir().unwrap();
    let engine = Engine::open(src.path(), 0).unwrap();
    seed(&engine, rows);

    let base = tempdir().unwrap();
    let t = Instant::now();
    engine.base_backup(base.path()).unwrap();
    let base_backup_ms = t.elapsed().as_secs_f64() * 1e3;

    // Post-base writes → archive.
    let pit_lsn = {
        let x = engine.begin().unwrap();
        for k in 0..500 {
            engine
                .execute_sql(
                    x,
                    &format!("INSERT INTO t (id, v) VALUES ({}, 1)", rows + k),
                )
                .unwrap();
        }
        engine.commit(x).unwrap();
        let lsn = engine.wal_current_lsn();
        let x = engine.begin().unwrap();
        for k in 0..500 {
            engine
                .execute_sql(
                    x,
                    &format!("INSERT INTO t (id, v) VALUES ({}, 2)", rows + 500 + k),
                )
                .unwrap();
        }
        engine.commit(x).unwrap();
        lsn
    };
    let archive = tempdir().unwrap();
    engine.archive_wal(archive.path()).unwrap();

    let dest = tempdir().unwrap();
    let t = Instant::now();
    unidb::backup::restore(base.path(), archive.path(), dest.path(), None).unwrap();
    let restore_ms = t.elapsed().as_secs_f64() * 1e3;

    let dest_pit = tempdir().unwrap();
    let t = Instant::now();
    unidb::backup::restore(base.path(), archive.path(), dest_pit.path(), Some(pit_lsn)).unwrap();
    let pitr_ms = t.elapsed().as_secs_f64() * 1e3;

    // ── replica: base + incremental WAL apply + failover ────────────────────
    let repl_src = tempdir().unwrap();
    let primary = Engine::open(repl_src.path(), 0).unwrap();
    seed(&primary, rows);
    primary.checkpoint().unwrap();
    let base_lsn = primary.wal_current_lsn();
    let repl_dir = tempdir().unwrap();
    let mut replica = Replica::init_from_base(repl_dir.path(), repl_src.path()).unwrap();

    // Post-base committed updates to existing rows (new MVCC versions land on
    // the base's pages' free space; a small batch stays within them — the
    // steady-state replica case, avoiding the documented fresh-page limit).
    let n_ship = 100usize;
    let x = primary.begin().unwrap();
    for k in 0..n_ship {
        engine_update(&primary, x, k % rows);
    }
    primary.commit(x).unwrap();
    let stream = primary.ship_wal(base_lsn).unwrap();
    let stream_bytes = stream.len();

    let t = Instant::now();
    replica
        .apply_stream(&stream, primary.primary_control())
        .unwrap();
    let apply_ms = t.elapsed().as_secs_f64() * 1e3;
    let apply_tps = n_ship as f64 / (apply_ms / 1e3);

    let t = Instant::now();
    let failover_ms = match replica.promote() {
        Ok(_) => t.elapsed().as_secs_f64() * 1e3,
        Err(e) => {
            println!("(promote hit the documented fresh-page limit at this scale: {e})");
            f64::NAN
        }
    };

    println!("| operation                       | time         |");
    println!("|---------------------------------|--------------|");
    println!("| base backup ({rows} rows)         | {base_backup_ms:8.1} ms |");
    println!("| restore to latest               | {restore_ms:8.1} ms |");
    println!("| PITR restore (to LSN)           | {pitr_ms:8.1} ms |");
    println!("| replica apply ({n_ship} updates)    | {apply_ms:8.1} ms ({apply_tps:.0} rows/s) |");
    println!("| WAL ship batch size             | {stream_bytes} bytes |");
    println!("| failover (promote → read-write) | {failover_ms:8.1} ms |");
}

fn engine_update(engine: &Engine, xid: u64, id: usize) {
    engine
        .execute_sql(xid, &format!("UPDATE t SET v = 7 WHERE id = {id}"))
        .unwrap();
}
