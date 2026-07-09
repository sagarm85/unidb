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

// ------------------------------ B2: CRUD -----------------------------------

/// Build a keyed unidb table of `n` rows with a BTREE index on `id`; return the
/// engine (kept alive by the caller) plus a prepared point-SELECT.
fn unidb_build_keyed(dir: &std::path::Path, n: u64) -> Engine {
    let engine = Engine::open(dir, 0).unwrap();
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE t (id INT, body TEXT)")
        .unwrap();
    engine
        .execute_sql(x, "CREATE INDEX ib ON t USING BTREE (id)")
        .unwrap();
    engine.commit(x).unwrap();
    // Bulk-load in BATCHED transactions — the size sweep builds tables of up to
    // millions of rows. Per-row durable commit (~3.5 ms each) would take hours;
    // one giant transaction overflows the heap's per-statement FSM at ~1e5+ rows
    // (a documented SQL-path bulk-insert limitation). Batching (commit every
    // BATCH rows) is both fast and correct — load speed is not what any keyed
    // test measures.
    const BATCH: u64 = 2_000;
    let ins = engine
        .prepare("INSERT INTO t (id, body) VALUES ($1, $2)")
        .unwrap();
    let mut x = engine.begin().unwrap();
    for i in 0..n {
        engine
            .execute_prepared(
                x,
                &ins,
                &[Literal::Int(i as i64), Literal::Text(format!("b{i}"))],
            )
            .unwrap();
        if (i + 1) % BATCH == 0 {
            engine.commit(x).unwrap();
            x = engine.begin().unwrap();
        }
    }
    engine.commit(x).unwrap();
    engine
}

/// Build a keyed Postgres table of `n` rows (PK on id) via a fast bulk load
/// (one transaction) — table content, not load speed, is what this sets up.
fn pg_build_keyed(client: &mut Client, n: u64) {
    client
        .batch_execute("DROP TABLE IF EXISTS t; CREATE TABLE t (id INT PRIMARY KEY, body TEXT)")
        .unwrap();
    let stmt = client
        .prepare("INSERT INTO t (id, body) VALUES ($1, $2)")
        .unwrap();
    let mut txn = client.transaction().unwrap();
    for i in 0..n {
        txn.execute(&stmt, &[&(i as i32), &format!("b{i}")])
            .unwrap();
    }
    txn.commit().unwrap();
}

/// B2: point SELECT by key, MVCC UPDATE, and churn-then-remeasure — unidb vs
/// both Postgres lenses. Reads don't fsync, so the lens is irrelevant to SELECT
/// throughput (we still run under lens 2, the matched-durability environment,
/// and note it); UPDATE and the churn re-measure are durability-sensitive.
fn bench_pg_crud(c: &mut Criterion) {
    let Some(url) = pg_url() else {
        eprintln!("[pg] PG_URL unset — skipping Postgres CRUD suite (B2)");
        return;
    };
    let mut group = c.benchmark_group("pg_crud");
    group.sample_size(20);
    group.throughput(Throughput::Elements(1));

    // ---- unidb (embedded, as-shipped default) ----
    let udir = tempdir().unwrap();
    let engine = unidb_build_keyed(udir.path(), KEYED_ROWS);
    let sel = engine
        .prepare("SELECT id, body FROM t WHERE id = $1")
        .unwrap();
    let upd = engine
        .prepare("UPDATE t SET body = $1 WHERE id = $2")
        .unwrap();
    // point SELECT by key (embedded index path — the no-IPC advantage)
    group.bench_function("unidb_point_select", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let x = engine.begin().unwrap();
            let key = (i % KEYED_ROWS) as i64;
            engine
                .execute_prepared(x, &sel, &[Literal::Int(key)])
                .unwrap();
            engine.commit(x).unwrap();
            i += 1;
        });
    });
    // MVCC UPDATE by key (new version + xmax stamp)
    group.bench_function("unidb_update", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let x = engine.begin().unwrap();
            let key = (i % KEYED_ROWS) as i64;
            engine
                .execute_prepared(
                    x,
                    &upd,
                    &[Literal::Text(format!("u{i}")), Literal::Int(key)],
                )
                .unwrap();
            engine.commit(x).unwrap();
            i += 1;
        });
    });
    drop(sel);
    drop(upd);
    drop(engine);
    drop(udir);

    // ---- unidb churn: read latency fresh vs after heavy update churn, and
    // after a manual VACUUM (M10) — the bloat-management maturity check ----
    unidb_churn_bench(&mut group);

    // ---- Postgres (lens 2, matched durability) ----
    let Some((mut client, method)) = pg_open_lens(&url, true) else {
        group.finish();
        return;
    };
    eprintln!("[pg] CRUD suite running under wal_sync_method={method} (lens 2)");
    pg_build_keyed(&mut client, KEYED_ROWS);
    let sel = client
        .prepare("SELECT id, body FROM t WHERE id = $1")
        .unwrap();
    let upd = client
        .prepare("UPDATE t SET body = $1 WHERE id = $2")
        .unwrap();
    group.bench_function(format!("pg_point_select [{method}]"), |b| {
        let mut i = 0i32;
        b.iter(|| {
            let key = i % KEYED_ROWS as i32;
            client.query(&sel, &[&key]).unwrap();
            i += 1;
        });
    });
    group.bench_function(format!("pg_update [{method}]"), |b| {
        let mut i = 0i32;
        b.iter(|| {
            let key = i % KEYED_ROWS as i32;
            client.execute(&upd, &[&format!("u{i}"), &key]).unwrap();
            i += 1;
        });
    });
    pg_churn_bench(&mut group, &mut client, &method);
    group.finish();
}

/// unidb churn: measure point-read latency on a fresh table, then after
/// `CHURN_ROUNDS` full-table update passes (heavy version accumulation), then
/// after a manual `Engine::vacuum()`. Fresh vs churned exposes MVCC bloat;
/// churned vs vacuumed shows M10 reclaiming it.
fn unidb_churn_bench(group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>) {
    const CHURN_ROUNDS: u64 = 30;
    let dir = tempdir().unwrap();
    let engine = unidb_build_keyed(dir.path(), KEYED_ROWS);
    let sel = engine
        .prepare("SELECT id, body FROM t WHERE id = $1")
        .unwrap();
    let read = |i: &mut u64| {
        let x = engine.begin().unwrap();
        let key = (*i % KEYED_ROWS) as i64;
        engine
            .execute_prepared(x, &sel, &[Literal::Int(key)])
            .unwrap();
        engine.commit(x).unwrap();
        *i += 1;
    };

    group.bench_function("unidb_read_fresh", |b| {
        let mut i = 0u64;
        b.iter(|| read(&mut i));
    });

    // Heavy update churn: rewrite every row CHURN_ROUNDS times.
    let upd = engine
        .prepare("UPDATE t SET body = $1 WHERE id = $2")
        .unwrap();
    for r in 0..CHURN_ROUNDS {
        for k in 0..KEYED_ROWS {
            let x = engine.begin().unwrap();
            engine
                .execute_prepared(
                    x,
                    &upd,
                    &[Literal::Text(format!("c{r}-{k}")), Literal::Int(k as i64)],
                )
                .unwrap();
            engine.commit(x).unwrap();
        }
    }
    group.bench_function("unidb_read_after_churn", |b| {
        let mut i = 0u64;
        b.iter(|| read(&mut i));
    });

    engine.vacuum().unwrap();
    group.bench_function("unidb_read_after_vacuum", |b| {
        let mut i = 0u64;
        b.iter(|| read(&mut i));
    });
}

/// Postgres churn: same shape (fresh read, then heavy update churn with
/// autovacuum on, then re-measure). No manual VACUUM — autovacuum is the point.
fn pg_churn_bench(
    group: &mut criterion::BenchmarkGroup<criterion::measurement::WallTime>,
    client: &mut Client,
    method: &str,
) {
    const CHURN_ROUNDS: u64 = 30;
    pg_build_keyed(client, KEYED_ROWS);
    let sel = client
        .prepare("SELECT id, body FROM t WHERE id = $1")
        .unwrap();
    group.bench_function(format!("pg_read_fresh [{method}]"), |b| {
        let mut i = 0i32;
        b.iter(|| {
            let key = i % KEYED_ROWS as i32;
            client.query(&sel, &[&key]).unwrap();
            i += 1;
        });
    });

    let upd = client
        .prepare("UPDATE t SET body = $1 WHERE id = $2")
        .unwrap();
    for r in 0..CHURN_ROUNDS {
        for k in 0..KEYED_ROWS as i32 {
            client.execute(&upd, &[&format!("c{r}-{k}"), &k]).unwrap();
        }
    }
    group.bench_function(format!("pg_read_after_churn [{method}]"), |b| {
        let mut i = 0i32;
        b.iter(|| {
            let key = i % KEYED_ROWS as i32;
            client.query(&sel, &[&key]).unwrap();
            i += 1;
        });
    });
}

// -------------------------- B3: concurrency --------------------------------

/// unidb raw-CRUD writers: N threads over one `Arc<Engine>`, each committing
/// `per_thread` single-row insert transactions. Returns committed txns/sec.
/// This is the path that scales (heap page latches + group commit).
fn unidb_raw_concurrency(writers: usize, per_thread: usize) -> f64 {
    let dir = tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    let barrier = Arc::new(Barrier::new(writers + 1));
    let committed = Arc::new(AtomicUsize::new(0));
    let payload = [0xABu8; 64];
    let mut handles = Vec::new();
    for _ in 0..writers {
        let (engine, barrier, committed) = (engine.clone(), barrier.clone(), committed.clone());
        handles.push(thread::spawn(move || {
            barrier.wait();
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
    committed.load(Ordering::Relaxed) as f64 / start.elapsed().as_secs_f64()
}

/// unidb SQL writers: N threads over one `Arc<Engine>`, each committing
/// `per_thread` single-row SQL INSERTs. This is the path the spec predicts
/// will NOT scale — every `execute_sql` takes the catalog RwLock in write mode
/// (documented Phase 5 limitation). Recorded regardless.
fn unidb_sql_concurrency(writers: usize, per_thread: usize) -> f64 {
    let dir = tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE t (id INT, body TEXT)")
        .unwrap();
    engine.commit(x).unwrap();
    let barrier = Arc::new(Barrier::new(writers + 1));
    let committed = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for w in 0..writers {
        let (engine, barrier, committed) = (engine.clone(), barrier.clone(), committed.clone());
        handles.push(thread::spawn(move || {
            let ins = engine
                .prepare("INSERT INTO t (id, body) VALUES ($1, $2)")
                .unwrap();
            barrier.wait();
            for i in 0..per_thread {
                let id = (w * per_thread + i) as i64;
                let xid = engine.begin().unwrap();
                engine
                    .execute_prepared(
                        xid,
                        &ins,
                        &[Literal::Int(id), Literal::Text(format!("b{id}"))],
                    )
                    .unwrap();
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
    committed.load(Ordering::Relaxed) as f64 / start.elapsed().as_secs_f64()
}

/// Postgres writers: N connections, each committing `per_thread` autocommit
/// single-row INSERTs (durable — lens 2). Returns committed txns/sec.
fn pg_concurrency(url: &str, writers: usize, per_thread: usize) -> Option<f64> {
    // Fresh table under lens 2.
    let (mut admin, _) = pg_open_lens(url, true)?;
    admin
        .batch_execute("DROP TABLE IF EXISTS c; CREATE TABLE c (id INT, body TEXT)")
        .unwrap();
    drop(admin);
    let barrier = Arc::new(Barrier::new(writers + 1));
    let committed = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for w in 0..writers {
        let (url, barrier, committed) = (url.to_string(), barrier.clone(), committed.clone());
        handles.push(thread::spawn(move || {
            let mut client = Client::connect(&url, NoTls).unwrap();
            let stmt = client
                .prepare("INSERT INTO c (id, body) VALUES ($1, $2)")
                .unwrap();
            barrier.wait();
            for i in 0..per_thread {
                let id = (w * per_thread + i) as i32;
                client.execute(&stmt, &[&id, &format!("b{id}")]).unwrap();
            }
            committed.fetch_add(per_thread, Ordering::Relaxed);
        }));
    }
    barrier.wait();
    let start = Instant::now();
    for h in handles {
        h.join().unwrap();
    }
    Some(committed.load(Ordering::Relaxed) as f64 / start.elapsed().as_secs_f64())
}

/// B3: concurrent-writer scaling at N ∈ {1,2,4,8}. Prints its own table (this
/// is the checkpoint most likely to produce the unflattering unidb-SQL number;
/// it ships regardless — spec prediction 3). Postgres columns are lens 2.
fn bench_pg_concurrency() {
    const PER: usize = 500;
    let url = pg_url();
    println!("\n=== B3: concurrent writers (commits/sec, higher is better) ===");
    if url.is_none() {
        println!("[pg] PG_URL unset — Postgres column skipped, unidb only");
    } else {
        println!("[pg] Postgres under lens 2 (fsync_writethrough)");
    }
    println!(
        "{:>6}  {:>16}  {:>16}  {:>16}",
        "N", "unidb_raw", "unidb_sql", "postgres"
    );
    let (mut raw_base, mut sql_base, mut pg_base) = (0.0f64, 0.0f64, 0.0f64);
    for &n in &[1usize, 2, 4, 8] {
        let raw = unidb_raw_concurrency(n, PER);
        let sql = unidb_sql_concurrency(n, PER);
        let pg = url.as_deref().and_then(|u| pg_concurrency(u, n, PER));
        if n == 1 {
            raw_base = raw;
            sql_base = sql;
            pg_base = pg.unwrap_or(0.0);
        }
        let fmt = |v: f64, base: f64| {
            if base > 0.0 {
                format!("{v:>10.0} ({:.2}x)", v / base)
            } else {
                format!("{v:>10.0}")
            }
        };
        let pg_s = match pg {
            Some(v) => fmt(v, pg_base),
            None => "        n/a".to_string(),
        };
        println!(
            "{:>6}  {:>16}  {:>16}  {:>16}",
            n,
            fmt(raw, raw_base),
            fmt(sql, sql_base),
            pg_s
        );
    }
}

fn main() {
    let mut criterion = Criterion::default().configure_from_args();
    bench_ladder(&mut criterion);
    bench_pg_ladder(&mut criterion);
    bench_pg_crud(&mut criterion);
    criterion.final_summary();
    bench_pg_concurrency();
}
