// Item 116 — Step-0 attribution for per-row-commit INSERT (Table 3's worst row).
//
// The Docker bench measures ~230 µs/row for unidb vs ~114 µs/row for Postgres,
// both at one fsync per row. Since PG's number bounds the shared fsync cost at
// ≲100 µs, unidb carries ~130–170 µs of its own software cost per commit.
//
// This probe runs the bench-identical path (sync commits — Engine::commit
// ALWAYS forces durability; deferred mode only moves the fsync into the
// group-commit window, it never skips it) and separates the platform fsync
// share via the WAL's fsync latency histogram: on macOS the F_FULLFSYNC floor
// is ~ms and would otherwise drown the software split we are attributing.
//
//   begin            — TransactionManager::begin (snapshot + WAL_TXN_BEGIN)
//   execute          — execute_prepared: plan → heap insert (page pick +
//                      write + WAL) → B-tree maintenance (+ its WAL)
//   commit − fsync   — undo bookkeeping, WAL_TXN_COMMIT append, group-commit
//                      coordination, lock release, hint bits
//
// Run:  ITEM116_N=100000 cargo test --release --test perf_item116 -- --nocapture

use std::sync::atomic::Ordering as AtomicOrd;
use std::time::Instant;
use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::sql::parallel_scan::{
    item116_reset, Q116_POST_NANOS, Q116_SYNC_NANOS, Q116_TXNMGR_NANOS,
};
use unidb::{AutoCheckpointConfig, Engine};

const GROUPS: i64 = 10;

fn build(engine: &Engine, rows: i64) {
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
        .unwrap();
    engine
        .execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
        .unwrap();
    engine.commit(x).unwrap();
    let ins = engine
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();
    let mut x = engine.begin().unwrap();
    for i in 0..rows {
        engine
            .execute_prepared(
                x,
                &ins,
                &[
                    Literal::Int(i),
                    Literal::Int(i),
                    Literal::Int(i % GROUPS),
                    Literal::Text(format!("b{i}")),
                ],
            )
            .unwrap();
        if (i + 1) % 5_000 == 0 {
            engine.commit(x).unwrap();
            x = engine.begin().unwrap();
        }
    }
    engine.commit(x).unwrap();
    let x = engine.begin().unwrap();
    let _ = engine.execute_sql(x, "ANALYZE t");
    engine.commit(x).unwrap();
}

#[test]
fn per_row_insert_phase_split() {
    let n: i64 = std::env::var("ITEM116_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let probe_rows: i64 = std::env::var("ITEM116_PROBE_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);

    let dir = tempdir().unwrap();
    let engine = Engine::open_with_pool_capacity(dir.path(), 0, 2_000_000).unwrap();
    engine.set_auto_checkpoint_config(AutoCheckpointConfig {
        max_wal_size: 512 * 1024 * 1024,
        ..Default::default()
    });
    build(&engine, n);
    // NOTE: no set_deferred_sync call — the engine DEFAULT (commit-time fsync,
    // one group-coalesced fsync per commit via Engine::commit's sync_up_to) is
    // exactly what the bench engine runs. Flipping to the legacy per-statement
    // mode here (as an earlier draft did) triples the fsync count and measures
    // a mode nothing ships with.

    let ins = engine
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();

    item116_reset();
    let wal0 = engine.wal_total_bytes_appended();
    let s0 = engine.stats();
    let fsync_us_0 = s0.wal_fsync_latency.mean_us * s0.wal_fsync_latency.count;
    let mut begin_ns: u64 = 0;
    let mut exec_ns: u64 = 0;
    let mut commit_ns: u64 = 0;
    let t_all = Instant::now();
    for i in 0..probe_rows {
        let id = n + i;
        let t0 = Instant::now();
        let xid = engine.begin().unwrap();
        let t1 = Instant::now();
        engine
            .execute_prepared(
                xid,
                &ins,
                &[
                    Literal::Int(id),
                    Literal::Int(id),
                    Literal::Int(id % GROUPS),
                    Literal::Text(format!("b{id}")),
                ],
            )
            .unwrap();
        let t2 = Instant::now();
        engine.commit(xid).unwrap();
        let t3 = Instant::now();
        begin_ns += (t1 - t0).as_nanos() as u64;
        exec_ns += (t2 - t1).as_nanos() as u64;
        commit_ns += (t3 - t2).as_nanos() as u64;
    }
    let total_us = t_all.elapsed().as_secs_f64() * 1e6 / probe_rows as f64;
    let wal_per_row = (engine.wal_total_bytes_appended() - wal0) / probe_rows as u64;
    let s1 = engine.stats();
    let fsync_us_1 = s1.wal_fsync_latency.mean_us * s1.wal_fsync_latency.count;
    let fsyncs = s1.wal_fsync_latency.count - s0.wal_fsync_latency.count;
    let fsync_us_row = fsync_us_1.saturating_sub(fsync_us_0) as f64 / probe_rows as f64;
    let pr = probe_rows as f64;
    let begin_us = begin_ns as f64 / 1e3 / pr;
    let exec_us = exec_ns as f64 / 1e3 / pr;
    let commit_us = commit_ns as f64 / 1e3 / pr;
    let commit_sw_us = (commit_us - fsync_us_row).max(0.0);
    let software_us = begin_us + exec_us + commit_sw_us;

    eprintln!();
    eprintln!("═══════════════════════════════════════════════════════════════════════");
    eprintln!(" Item 116 Step-0 — per-row INSERT phase split ({n} rows pre-loaded)");
    eprintln!("═══════════════════════════════════════════════════════════════════════");
    eprintln!(
        " bench-identical sync mode, {probe_rows} rows · WAL {wal_per_row} B/row · {fsyncs} fsyncs"
    );
    eprintln!(" per-row total              : {total_us:>8.2} µs");
    eprintln!(
        " ── fsync (histogram)       : {fsync_us_row:>8.2} µs  ({:>4.1}%)  [platform floor]",
        fsync_us_row / total_us * 100.0
    );
    eprintln!(" ── SOFTWARE total          : {software_us:>8.2} µs");
    eprintln!("     · begin (snapshot+WAL) : {begin_us:>8.2} µs");
    eprintln!("     · execute (heap+idx+WAL): {exec_us:>8.2} µs");
    eprintln!("     · commit minus fsync   : {commit_sw_us:>8.2} µs");
    let txnmgr_us = Q116_TXNMGR_NANOS.load(AtomicOrd::Relaxed) as f64 / 1e3 / pr;
    let sync_us = Q116_SYNC_NANOS.load(AtomicOrd::Relaxed) as f64 / 1e3 / pr;
    let post_us = Q116_POST_NANOS.load(AtomicOrd::Relaxed) as f64 / 1e3 / pr;
    eprintln!("       - txn_mgr.commit (undo+WAL append+locks): {txnmgr_us:>8.2} µs");
    eprintln!(
        "       - sync_up_to (incl fsync wait)          : {sync_us:>8.2} µs  (sw ≈ {:.2})",
        (sync_us - fsync_us_row).max(0.0)
    );
    eprintln!("       - post (timeline/deltas/wake/autockpt)  : {post_us:>8.2} µs");
    eprintln!("═══════════════════════════════════════════════════════════════════════");
    eprintln!();
}
