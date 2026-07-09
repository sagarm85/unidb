// W0–W4 decomposition ladder: attribute per-commit write cost one layer at a
// time, so "where does the multi-model write tax come from?" is answered by
// subtraction instead of instinct:
//
//   W0  plain row insert, no secondary index   -> the base engine (WAL+heap+fsync)
//   W1  + B-tree secondary index               -> W1-W0 = B-tree maintenance
//   W2  + VECTOR(128) column + IVF index       -> W2-W1 = vector-index maintenance
//   W3  + graph edge per insert                -> W3-W2 = edge write + adjacency index
//   W4  + event capture (full signature op)    -> W4-W0 = total multi-model tax
//
// W4-W0 is the decision gate for the async-derivation design (the maximum a
// log-derived background applier could ever take off the commit path). SQLite
// baselines (rusqlite, WAL journal + synchronous=FULL) anchor W0/W1 against
// the honest embedded incumbent per CLAUDE.md §6; W2+ have no SQLite analog.
//
// Every rung commits each row in its own transaction (begin -> work -> commit,
// one durable group-commit fsync each), unlike benches/vector.rs's one-txn bulk
// load — the ladder measures the *per-commit* cost the async design targets.
// Inserts go through prepared statements so SQL parse cost is identical across
// rungs and never pollutes a delta.
//
// Run with: cargo bench --bench decompose

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rusqlite::Connection;
use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::Engine;

const DIM: usize = 128;
const ROWS: u64 = 100;

fn embedding(seed: u64) -> Vec<f32> {
    (0..DIM)
        .map(|i| ((seed as f32) * 0.001 + i as f32).sin())
        .collect()
}

/// One ladder iteration at `rung`: fresh engine, schema per the rung, then
/// `n` durable single-row transactions carrying the rung's full workload.
///
/// `one_fsync` selects deferred-sync mode: statement mini-txns skip their
/// per-call fsync and `Engine::commit`'s `sync_up_to` issues the single
/// commit-time fsync — the "one durable point per user transaction" mode the
/// engine already has (M9/P5.e-3). Commits are exactly as durable (the txn is
/// not acknowledged until its commit LSN is synced); what's given up is only
/// mid-transaction statement durability, which ACID never promised anyway.
fn run_unidb_rung(rung: u8, one_fsync: bool, n: u64) {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    if one_fsync {
        engine.set_deferred_sync(true);
    }

    let setup = engine.begin().unwrap();
    let create = if rung >= 2 {
        "CREATE TABLE t (id INT, body TEXT, embedding VECTOR(128))"
    } else {
        "CREATE TABLE t (id INT, body TEXT)"
    };
    engine.execute_sql(setup, create).unwrap();
    if rung >= 1 {
        engine
            .execute_sql(setup, "CREATE INDEX ib ON t USING BTREE (id)")
            .unwrap();
    }
    if rung >= 2 {
        engine
            .execute_sql(setup, "CREATE INDEX iv ON t USING HNSW (embedding)")
            .unwrap();
    }
    engine.commit(setup).unwrap();
    if rung >= 4 {
        engine.enable_events("t").unwrap();
    }

    let ins = if rung >= 2 {
        engine
            .prepare("INSERT INTO t (id, body, embedding) VALUES ($1, $2, $3)")
            .unwrap()
    } else {
        engine
            .prepare("INSERT INTO t (id, body) VALUES ($1, $2)")
            .unwrap()
    };

    for i in 0..n {
        let xid = engine.begin().unwrap();
        let mut params = vec![Literal::Int(i as i64), Literal::Text(format!("body-{i}"))];
        if rung >= 2 {
            params.push(Literal::Vector(embedding(i)));
        }
        engine.execute_prepared(xid, &ins, &params).unwrap();
        if rung >= 3 {
            engine
                .create_edge(xid, i as i64, (i as i64) + 1, "rel", "{}")
                .unwrap();
        }
        engine.commit(xid).unwrap();
    }
}

/// SQLite baseline, durability-matched: WAL journal + synchronous=FULL +
/// **fullfsync=ON**, one autocommit (durable) transaction per row, prepared
/// statement. `fullfsync` matters on macOS: Rust's `File::sync_all` (unidb's
/// commit sync) issues `F_FULLFSYNC` (true flush-to-platter), while SQLite's
/// plain `synchronous=FULL` uses `fsync()`, which macOS does NOT make durable.
/// Without this pragma SQLite looks ~100x faster purely by syncing less.
fn run_sqlite(with_index: bool, n: u64) {
    let dir = tempdir().unwrap();
    let conn = Connection::open(dir.path().join("s.db")).unwrap();
    let _mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    conn.pragma_update(None, "synchronous", "FULL").unwrap();
    conn.pragma_update(None, "fullfsync", "ON").unwrap();
    conn.execute_batch("CREATE TABLE t (id INTEGER, body TEXT)")
        .unwrap();
    if with_index {
        conn.execute_batch("CREATE INDEX ib ON t(id)").unwrap();
    }
    let mut stmt = conn
        .prepare("INSERT INTO t (id, body) VALUES (?1, ?2)")
        .unwrap();
    for i in 0..n {
        stmt.execute(rusqlite::params![i as i64, format!("body-{i}")])
            .unwrap();
    }
}

fn bench_ladder(c: &mut Criterion) {
    let mut group = c.benchmark_group("decompose");
    group.sample_size(10);
    group.throughput(Throughput::Elements(ROWS));

    for (name, rung) in [
        ("w0_row", 0u8),
        ("w1_btree", 1),
        ("w2_vector", 2),
        ("w3_edge", 3),
        ("w4_event_full", 4),
    ] {
        group.bench_with_input(BenchmarkId::new(name, ROWS), &rung, |b, &r| {
            b.iter(|| run_unidb_rung(r, false, ROWS));
        });
    }

    // The decisive counterfactual: the same W0/W4 workloads with ONE fsync per
    // user transaction (deferred statement sync + commit-time sync_up_to).
    // (W4 - w4_1fsync) is the pure fsync-multiplication tax; what remains of
    // (w4_1fsync - w0_1fsync) is the true index/CPU work an async applier
    // could still move off the commit path.
    for (name, rung) in [("w0_1fsync", 0u8), ("w4_1fsync", 4)] {
        group.bench_with_input(BenchmarkId::new(name, ROWS), &rung, |b, &r| {
            b.iter(|| run_unidb_rung(r, true, ROWS));
        });
    }

    group.bench_with_input(BenchmarkId::new("sqlite_w0", ROWS), &(), |b, _| {
        b.iter(|| run_sqlite(false, ROWS));
    });
    group.bench_with_input(BenchmarkId::new("sqlite_w1", ROWS), &(), |b, _| {
        b.iter(|| run_sqlite(true, ROWS));
    });

    group.finish();
}

criterion_group!(benches, bench_ladder);
criterion_main!(benches);
