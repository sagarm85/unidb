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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput};
use postgres::{Client, NoTls};
use rusqlite::Connection;
use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::Engine;

const DIM: usize = 128;
const ROWS: u64 = 100;

/// Table size for the keyed CRUD tests (point SELECT / UPDATE / churn).
const KEYED_ROWS: u64 = 1_000;

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

// ---------------------------------------------------------------------------
// Postgres baseline comparison (spec: docs/backlog/pg_baseline_comparison.md).
//
// A fitness check — engine vs engine, both as shipped, on the CRUD both can do.
// PG_URL-gated: when unset, every Postgres path logs a skip line and returns, so
// a plain `cargo bench` is unaffected. When set, we report TWO durability lenses
// side by side, never one alone (the spec's core honesty rule):
//
//   lens 1 "default"  — wal_sync_method = open_datasync (macOS PG default, NOT
//                       flush-to-platter durable on macOS)
//   lens 2 "durable"  — wal_sync_method = fsync_writethrough (F_FULLFSYNC),
//                       matching unidb's Rust File::sync_all default. Headline
//                       numbers come from this lens.
//
// wal_sync_method is a `sighup` GUC (not settable per-session), so we flip it
// server-wide via ALTER SYSTEM + pg_reload_conf() (superuser; native local
// setup) and VERIFY it took effect with SHOW — the printed bench id carries the
// actual method in force, so a mislabelled number is impossible.
// ---------------------------------------------------------------------------

/// The `PG_URL` connection string, or `None` (→ skip the Postgres paths).
fn pg_url() -> Option<String> {
    std::env::var("PG_URL").ok()
}

fn pg_connect(url: &str) -> Option<Client> {
    match Client::connect(url, NoTls) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("  [pg] WARNING: PG_URL set but connect failed ({e}) — skipping");
            None
        }
    }
}

/// Flip the server-wide durability lens, then open a fresh work connection that
/// is guaranteed to observe the reloaded setting. Returns `(client, method)`
/// where `method` is the `wal_sync_method` actually in force (verified). `None`
/// on any failure (unreachable, or non-superuser → ALTER SYSTEM denied), after
/// logging — the caller then skips this config cleanly.
fn pg_open_lens(url: &str, durable: bool) -> Option<(Client, String)> {
    let want = if durable {
        "fsync_writethrough"
    } else {
        "open_datasync"
    };
    let mut admin = pg_connect(url)?;
    if let Err(e) = admin.batch_execute(&format!("ALTER SYSTEM SET wal_sync_method = '{want}'")) {
        eprintln!("  [pg] WARNING: ALTER SYSTEM denied ({e}) — need superuser; skipping lens");
        return None;
    }
    admin.batch_execute("SELECT pg_reload_conf()").ok()?;
    drop(admin);
    // pg_reload_conf() signals the postmaster; a *fresh* backend is the reliable
    // way to read the applied value. Small settle delay first.
    thread::sleep(Duration::from_millis(600));
    let mut client = pg_connect(url)?;
    let actual: String = client.query_one("SHOW wal_sync_method", &[]).ok()?.get(0);
    if actual != want {
        eprintln!("  [pg] WARNING: wanted wal_sync_method={want}, got {actual} — labelling actual");
    }
    Some((client, actual))
}

// ------------------------------ B1: ladder ---------------------------------

/// One Postgres ladder iteration: TRUNCATE, then `n` autocommit single-row
/// INSERTs via a prepared statement (each its own durable transaction — matches
/// unidb's per-row begin/insert/commit). Schema is created once by the caller.
fn pg_run_ladder(client: &mut Client, stmt: &postgres::Statement, n: u64) {
    client.batch_execute("TRUNCATE t").unwrap();
    for i in 0..n {
        client
            .execute(stmt, &[&(i as i32), &format!("body-{i}")])
            .unwrap();
    }
}

/// Register the four B1 configs (pg_w0/w1 × default/durable) plus the unidb W0/W1
/// counterparts re-measured here so the group reads side by side.
fn bench_pg_ladder(c: &mut Criterion) {
    let Some(url) = pg_url() else {
        eprintln!("[pg] PG_URL unset — skipping Postgres ladder (B1)");
        return;
    };
    let mut group = c.benchmark_group("pg_ladder");
    group.sample_size(10);
    group.throughput(Throughput::Elements(ROWS));

    // unidb side (as-shipped default = group-committed force-log-at-commit).
    for (name, rung) in [("unidb_w0", 0u8), ("unidb_w1", 1u8)] {
        group.bench_function(BenchmarkId::new(name, ROWS), |b| {
            b.iter(|| run_unidb_rung(rung, false, ROWS));
        });
    }

    // Postgres side, both lenses. w0 = PRIMARY KEY table (spec: a PG table always
    // has a PK); w1 = + a secondary btree, so w1-w0 = secondary-index maintenance.
    for (name, durable, with_index) in [
        ("pg_w0_default", false, false),
        ("pg_w1_default", false, true),
        ("pg_w0_durable", true, false),
        ("pg_w1_durable", true, true),
    ] {
        let Some((mut client, method)) = pg_open_lens(&url, durable) else {
            continue;
        };
        client
            .batch_execute("DROP TABLE IF EXISTS t; CREATE TABLE t (id INT PRIMARY KEY, body TEXT)")
            .unwrap();
        if with_index {
            client.batch_execute("CREATE INDEX ib ON t (body)").unwrap();
        }
        let stmt = client
            .prepare("INSERT INTO t (id, body) VALUES ($1, $2)")
            .unwrap();
        group.bench_function(BenchmarkId::new(format!("{name} [{method}]"), ROWS), |b| {
            b.iter(|| pg_run_ladder(&mut client, &stmt, ROWS));
        });
    }
    group.finish();
}

fn main() {
    let mut criterion = Criterion::default().configure_from_args();
    bench_ladder(&mut criterion);
    bench_pg_ladder(&mut criterion);
    criterion.final_summary();
}
