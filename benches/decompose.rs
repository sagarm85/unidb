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

// --------------------------- B4: size sweep --------------------------------

/// B4: does anything bend with table size? Build to size S via bulk load, then
/// measure marginal durable-insert throughput and point-read latency at that
/// size, for both engines. Sizes are env-overridable (`PG_SWEEP_SIZES`, comma
/// list) so a plain `cargo bench` stays quick; the script drives the full run.
fn bench_size_sweep() {
    let sizes: Vec<u64> = std::env::var("PG_SWEEP_SIZES")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![10_000, 100_000]);
    let url = pg_url();
    println!("\n=== B4: size sweep (insert µs/op, point-read µs/op) ===");
    if url.is_none() {
        println!("[pg] PG_URL unset — Postgres columns skipped, unidb only");
    }
    println!(
        "{:>10}  {:>22}  {:>22}",
        "rows", "unidb ins/read µs", "pg ins/read µs (lens2)"
    );
    // Set lens 2 once for the whole sweep.
    let mut pg_lens: Option<(Client, String)> = url.as_deref().and_then(|u| pg_open_lens(u, true));
    for &s in &sizes {
        let (u_ins, u_read) = unidb_sweep_point(s);
        let pg_cell = match pg_lens.as_mut() {
            Some((client, _)) => {
                let (p_ins, p_read) = pg_sweep_point(client, s);
                format!("{p_ins:>9.1} / {p_read:>9.1}")
            }
            None => "            n/a".to_string(),
        };
        println!(
            "{:>10}  {:>9.1} / {:>9.1}      {:>22}",
            s, u_ins, u_read, pg_cell
        );
    }
}

const SWEEP_SAMPLE: u64 = 200;

/// Build a unidb heap of `size` rows via the RAW CRUD path, then return
/// (marginal durable insert µs/op, point-read µs/op) at that size.
///
/// Why the raw path (not SQL) here: this test *is* the P1.c flatness claim
/// (`benches/scale.rs`), and the SQL insert path's per-statement lazy FSM caps
/// bulk SQL loads at ~145k rows in one transaction (a documented limitation —
/// `Engine::insert` keeps the FSM warm and scales, SQL does not). Point reads
/// are raw `get(row_id)` — the embedded no-IPC read compared against Postgres's
/// keyed SELECT. Load itself is batched (undo/WAL bounded); the *measured* ops
/// are per-row durable.
fn unidb_sweep_point(size: u64) -> (f64, f64) {
    const BATCH: u64 = 5_000;
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let mut rids = Vec::with_capacity(size as usize);
    let mut x = engine.begin().unwrap();
    for i in 0..size {
        rids.push(engine.insert(x, &i.to_le_bytes()).unwrap());
        if (i + 1) % BATCH == 0 {
            engine.commit(x).unwrap();
            x = engine.begin().unwrap();
        }
    }
    engine.commit(x).unwrap();

    // marginal durable insert throughput at this size
    let start = Instant::now();
    for j in 0..SWEEP_SAMPLE {
        let x = engine.begin().unwrap();
        engine.insert(x, &(size + j).to_le_bytes()).unwrap();
        engine.commit(x).unwrap();
    }
    let ins_us = start.elapsed().as_micros() as f64 / SWEEP_SAMPLE as f64;

    // point read (embedded get by row id) at this size
    let step = (size / SWEEP_SAMPLE).max(1);
    let start = Instant::now();
    for j in 0..SWEEP_SAMPLE {
        let rid = rids[((j * step) % size) as usize];
        let x = engine.begin().unwrap();
        engine.get(x, rid).unwrap();
        engine.commit(x).unwrap();
    }
    let read_us = start.elapsed().as_micros() as f64 / SWEEP_SAMPLE as f64;
    (ins_us, read_us)
}

/// Same for Postgres (lens set by the caller). Bulk-builds to `size`, then
/// measures marginal durable insert + point read.
fn pg_sweep_point(client: &mut Client, size: u64) -> (f64, f64) {
    pg_build_keyed(client, size);
    let ins = client
        .prepare("INSERT INTO t (id, body) VALUES ($1, $2)")
        .unwrap();
    let start = Instant::now();
    for j in 0..SWEEP_SAMPLE {
        let id = (size + j) as i32;
        client.execute(&ins, &[&id, &format!("b{id}")]).unwrap();
    }
    let ins_us = start.elapsed().as_micros() as f64 / SWEEP_SAMPLE as f64;
    let sel = client
        .prepare("SELECT id, body FROM t WHERE id = $1")
        .unwrap();
    let start = Instant::now();
    for j in 0..SWEEP_SAMPLE {
        let key = (j * (size / SWEEP_SAMPLE).max(1)) as i32 % size as i32;
        client.query(&sel, &[&key]).unwrap();
    }
    let read_us = start.elapsed().as_micros() as f64 / SWEEP_SAMPLE as f64;
    (ins_us, read_us)
}

/// Durable-FSM B-accept: marginal SQL-insert cost as a single table grows, in
/// one open transaction (so there is no per-row fsync masking the CPU cost). The
/// segment µs/row includes each page-growth's amortized bookkeeping. On `main`
/// (before), every growth rewrote the whole `TableDef.pages` list into the
/// catalog blob (O(pages)), and at ~1,450 pages that blob overflowed an 8 KiB
/// page → `HeapFull { size: 8138 }` (printed as ERROR — the ceiling). With the
/// durable FSM (after), the directory lives in the FSM tree (an O(log n) tail
/// probe and one node write per new page), so the cost stays flat and the build
/// sails past the old ceiling. Rows overridable via `FSM_ROWS`.
fn bench_fsm_scale() {
    use unidb::sql::logical::Literal;
    println!(
        "\n=== FSM B-accept: marginal SQL-insert cost vs table size (one txn, ~4 rows/page) ==="
    );
    println!(
        "  {:>8}  {:>8}  {:>14}  status",
        "rows", "~pages", "us/row(seg)"
    );
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let x0 = engine.begin().unwrap();
    engine
        .execute_sql(x0, "CREATE TABLE t (id INT, body TEXT)")
        .unwrap();
    engine.commit(x0).unwrap();
    let body = "x".repeat(1900); // ~4 rows per 8 KiB page
    let ins = engine
        .prepare("INSERT INTO t (id, body) VALUES ($1, $2)")
        .unwrap();
    let target: u64 = std::env::var("FSM_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8_000);
    let milestone = 1_000u64;
    let x = engine.begin().unwrap();
    let mut seg_start = Instant::now();
    let mut seg_rows = 0u64;
    for i in 0..target {
        match engine.execute_prepared(
            x,
            &ins,
            &[Literal::Int(i as i64), Literal::Text(body.clone())],
        ) {
            Ok(_) => {}
            Err(e) => {
                println!("  {:>8}  {:>8}  {:>14}  ERROR: {e}", i, i / 4, "-");
                return; // the ceiling (before) — no commit; txn is abandoned
            }
        }
        seg_rows += 1;
        if (i + 1) % milestone == 0 {
            let us = seg_start.elapsed().as_micros() as f64 / seg_rows as f64;
            println!("  {:>8}  {:>8}  {:>14.1}  ok", i + 1, (i + 1) / 4, us);
            seg_start = Instant::now();
            seg_rows = 0;
        }
    }
    engine.commit(x).unwrap();
}

/// Concurrent SQL writers over a table **pre-grown** to `pregrow_rows` wide rows
/// (so the heap already spans many pages before the measured phase). The
/// measured phase is the same as `unidb_sql_concurrency`: N writers, each
/// committing `per_thread` durable single-row INSERTs. Isolates one variable —
/// does concurrent-write throughput hold as the table gets large? On `main`
/// (before), each measured INSERT rebuilds `Heap::from_pages(pages.clone())`
/// (O(pages)) and, on any page growth, rewrites the whole page list into the
/// catalog under the write lock; after, `Heap::open` is O(1) and growth is an
/// O(log n) FSM write with no catalog lock. Returns commits/sec of the measured
/// phase only (the pre-grow is not timed).
fn unidb_sql_conc_pregrown(writers: usize, per_thread: usize, pregrow_rows: u64) -> f64 {
    let dir = tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE t (id INT, body TEXT)")
        .unwrap();
    engine.commit(x).unwrap();
    if pregrow_rows > 0 {
        let wide = "w".repeat(1900); // ~4 rows per 8 KiB page → many pages fast
        let ins = engine
            .prepare("INSERT INTO t (id, body) VALUES ($1, $2)")
            .unwrap();
        let mut x = engine.begin().unwrap();
        for i in 0..pregrow_rows {
            engine
                .execute_prepared(
                    x,
                    &ins,
                    &[Literal::Int(-(i as i64) - 1), Literal::Text(wide.clone())],
                )
                .unwrap();
            if (i + 1) % 2000 == 0 {
                engine.commit(x).unwrap();
                x = engine.begin().unwrap();
            }
        }
        engine.commit(x).unwrap();
    }
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

/// High-scale concurrency: 8-writer commits/sec as the table is pre-grown to
/// increasing sizes. `FSM_PREGROW_SIZES` (comma list of pre-grow row counts)
/// overrides the sweep — `main` (before) can only reach sizes below the ~876-page
/// ceiling, so run it with e.g. `FSM_PREGROW_SIZES=0,2000`; the after binary runs
/// the full sweep to show whether throughput stays flat at scales before could
/// not reach at all.
fn bench_conc_scale() {
    const W: usize = 8;
    const PER: usize = 300;
    println!("\n=== Concurrent SQL writes vs pre-grown table size ({W} writers, PER={PER}, commits/sec) ===");
    println!(
        "  {:>12}  {:>8}  {:>16}",
        "pregrow_rows", "~pages", "commits/sec"
    );
    let sizes: Vec<u64> = std::env::var("FSM_PREGROW_SIZES")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![0, 2_000, 8_000, 30_000]);
    for &pg in &sizes {
        let tput = unidb_sql_conc_pregrown(W, PER, pg);
        println!("  {:>12}  {:>8}  {:>16.0}", pg, pg / 4, tput);
    }
}

// ----------------- High-scale concurrency experiment (millions) ------------
//
// The first "millions of records" concurrency run. It exists to answer two
// questions with data rather than instinct:
//
//   Q1 — Do concurrent SQL writes scale at high table sizes, and how much
//        headroom is there vs the raw path (which already scales)? The raw
//        column is "what good looks like"; the gap is the prize for fixing the
//        SQL path.
//   Q2 — Is today's binding constraint the per-statement catalog `RwLock` or
//        B-tree latch contention? (i.e. would latch-coupled "crabbing" B-tree
//        descent help *now*, or only after the catalog lock is split?)
//
// All three tables share the same measured phase: N writer threads over one
// `Arc<Engine>`, each committing `per` durable single-row INSERTs, group-commit
// on. The tables are pre-grown ONCE to a large size so page count / tree depth
// are realistic; the pre-grow is not timed.

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Bulk-load a SQL table to `rows` rows through the SQL insert path (batched
/// commits so undo/WAL stay bounded). `body_len` sets row width. If `indexed`,
/// a secondary B-tree index on `k` is maintained on every insert.
fn build_sql_table(engine: &Arc<Engine>, rows: u64, body_len: usize, indexed: bool) {
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE t (id INT, k INT, body TEXT)")
        .unwrap();
    if indexed {
        engine
            .execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
            .unwrap();
    }
    engine.commit(x).unwrap();
    let body = "d".repeat(body_len);
    let ins = engine
        .prepare("INSERT INTO t (id, k, body) VALUES ($1, $2, $3)")
        .unwrap();
    let mut x = engine.begin().unwrap();
    for i in 0..rows {
        engine
            .execute_prepared(
                x,
                &ins,
                &[
                    Literal::Int(-(i as i64) - 1),
                    Literal::Int(i as i64),
                    Literal::Text(body.clone()),
                ],
            )
            .unwrap();
        if (i + 1) % 5_000 == 0 {
            engine.commit(x).unwrap();
            x = engine.begin().unwrap();
        }
    }
    engine.commit(x).unwrap();
}

/// Measured phase: `writers` threads each commit `per` durable single-row SQL
/// INSERTs into the pre-grown `t`. `id_base` keeps ids distinct across repeated
/// runs on the same engine. Returns commits/sec.
fn measure_sql_writers(engine: &Arc<Engine>, writers: usize, per: usize, id_base: i64) -> f64 {
    let barrier = Arc::new(Barrier::new(writers + 1));
    let committed = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for w in 0..writers {
        let (engine, barrier, committed) = (engine.clone(), barrier.clone(), committed.clone());
        handles.push(thread::spawn(move || {
            let ins = engine
                .prepare("INSERT INTO t (id, k, body) VALUES ($1, $2, $3)")
                .unwrap();
            barrier.wait();
            for i in 0..per {
                let id = id_base + (w * per + i) as i64;
                let xid = engine.begin().unwrap();
                engine
                    .execute_prepared(
                        xid,
                        &ins,
                        &[
                            Literal::Int(id),
                            Literal::Int(id),
                            Literal::Text(format!("b{id}")),
                        ],
                    )
                    .unwrap();
                engine.commit(xid).unwrap();
            }
            committed.fetch_add(per, Ordering::Relaxed);
        }));
    }
    barrier.wait();
    let start = Instant::now();
    for h in handles {
        h.join().unwrap();
    }
    committed.load(Ordering::Relaxed) as f64 / start.elapsed().as_secs_f64()
}

/// Measured phase for the RAW CRUD path over a pre-grown heap: `writers` threads
/// each commit `per` durable single-row `Engine::insert`s. Returns commits/sec.
fn measure_raw_writers(engine: &Arc<Engine>, writers: usize, per: usize) -> f64 {
    let barrier = Arc::new(Barrier::new(writers + 1));
    let committed = Arc::new(AtomicUsize::new(0));
    let payload = [0xCDu8; 64];
    let mut handles = Vec::new();
    for _ in 0..writers {
        let (engine, barrier, committed) = (engine.clone(), barrier.clone(), committed.clone());
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..per {
                let xid = engine.begin().unwrap();
                engine.insert(xid, &payload).unwrap();
                engine.commit(xid).unwrap();
            }
            committed.fetch_add(per, Ordering::Relaxed);
        }));
    }
    barrier.wait();
    let start = Instant::now();
    for h in handles {
        h.join().unwrap();
    }
    committed.load(Ordering::Relaxed) as f64 / start.elapsed().as_secs_f64()
}

/// Put the server on the matched-durability lens (lens 2, `fsync_writethrough`
/// = F_FULLFSYNC, the only macOS setting that truly flushes to platter — the
/// honest apples-to-apples vs unidb's `File::sync_all`). Returns the
/// `wal_sync_method` actually in force (for labelling), or `None` to skip. This
/// is a SERVER-WIDE `ALTER SYSTEM` change; the caller MUST call [`pg_reset_lens`]
/// afterward so the user's Postgres is left as it was found. Set once per run.
fn pg_ensure_lens(url: &str) -> Option<String> {
    let (admin, method) = pg_open_lens(url, true)?;
    drop(admin);
    Some(method)
}

/// Build a pre-grown Postgres table `name` (dropping any prior one), optionally
/// with a secondary B-tree index on `k`. Pre-grow is one server-side
/// `generate_series` load (one txn, one fsync), so it is fast even at millions.
/// Assumes [`pg_ensure_lens`] already set the durability lens. Returns `None`
/// on any failure (→ the caller skips the Postgres cell cleanly).
fn pg_build_table(url: &str, name: &str, rows: u64, indexed: bool) -> Option<()> {
    let mut c = pg_connect(url)?;
    c.batch_execute(&format!(
        "DROP TABLE IF EXISTS {name}; CREATE TABLE {name} (id BIGINT, k BIGINT, body TEXT)"
    ))
    .ok()?;
    if indexed {
        c.batch_execute(&format!("CREATE INDEX {name}_k ON {name} (k)"))
            .ok()?;
    }
    if rows > 0 {
        c.execute(
            &format!(
                "INSERT INTO {name} (id, k, body) \
                 SELECT -g - 1, g, repeat('d', 96) FROM generate_series(0, $1::bigint - 1) g"
            ),
            &[&(rows as i64)],
        )
        .ok()?;
    }
    Some(())
}

/// Measured phase for Postgres over the pre-grown table `name`: `writers`
/// connections, each committing `per` durable autocommit single-row INSERTs.
/// `id_base` keeps ids distinct across repeated runs. Returns commits/sec.
fn pg_measure_table(
    url: &str,
    name: &str,
    writers: usize,
    per: usize,
    id_base: i64,
) -> Option<f64> {
    let barrier = Arc::new(Barrier::new(writers + 1));
    let committed = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for w in 0..writers {
        let (url, name, barrier, committed) = (
            url.to_string(),
            name.to_string(),
            barrier.clone(),
            committed.clone(),
        );
        handles.push(thread::spawn(move || {
            let mut client = Client::connect(&url, NoTls).unwrap();
            let ins = client
                .prepare(&format!(
                    "INSERT INTO {name} (id, k, body) VALUES ($1, $2, $3)"
                ))
                .unwrap();
            barrier.wait();
            for i in 0..per {
                let id = id_base + (w * per + i) as i64;
                client
                    .execute(&ins, &[&id, &id, &format!("b{id}")])
                    .unwrap();
            }
            committed.fetch_add(per, Ordering::Relaxed);
        }));
    }
    barrier.wait();
    let start = Instant::now();
    for h in handles {
        h.join().unwrap();
    }
    Some(committed.load(Ordering::Relaxed) as f64 / start.elapsed().as_secs_f64())
}

/// Undo the server-wide lens change from [`pg_ensure_lens`], restoring the
/// default `wal_sync_method`. Best-effort — a failure here only means the setting
/// stays at lens 2, which the run already reported.
fn pg_reset_lens(url: &str) {
    if let Some(mut c) = pg_connect(url) {
        let _ = c.batch_execute("ALTER SYSTEM RESET wal_sync_method");
        let _ = c.batch_execute("SELECT pg_reload_conf()");
    }
}

fn bench_hiconc() {
    let pregrow = env_u64("HICONC_PREGROW", 2_000_000);
    let per = env_u64("HICONC_PER", 400) as usize;
    let idx_pregrow = env_u64("HICONC_IDX_PREGROW", 200_000);
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let writers = [1usize, 2, 4, 8];
    // Optional selector, e.g. HICONC_ONLY=c to re-run just Table C.
    let only = std::env::var("HICONC_ONLY").unwrap_or_default();
    let run = |t: &str| only.is_empty() || only.contains(t);
    println!("\n########## HIGH-SCALE CONCURRENCY EXPERIMENT ({cores} logical cores) ##########");
    println!(
        "pregrow(sql/raw)={pregrow} rows, indexed pregrow={idx_pregrow} rows, per-writer burst={per} commits, group-commit on\n"
    );

    // Postgres comparison is optional (PG_URL-gated) and shares ONE server-wide
    // durability-lens change across all three tables: set here, reset at the end.
    let url = pg_url();
    let pg_method = match url.as_deref() {
        Some(u) => pg_ensure_lens(u),
        None => {
            println!("  [pg] PG_URL unset — Postgres columns skipped\n");
            None
        }
    };
    if let Some(ref m) = pg_method {
        println!(
            "  [pg] Postgres durability lens: wal_sync_method={m} (reset to default at end)\n"
        );
    }
    // Some(&str) only when the lens is genuinely in force, so a cell is emitted.
    let pg_on = || pg_method.as_ref().and(url.as_deref());

    // ---- Table A: SQL vs raw writer-count scaling at a large table ----
    if run("a") {
        println!(
            "=== A. Writer-count scaling at {pregrow}-row table (commits/sec, speedup vs 1) ==="
        );
        let sql_dir = tempdir().unwrap();
        let sql_engine = Arc::new(Engine::open(sql_dir.path(), 0).unwrap());
        sql_engine.set_deferred_sync(true);
        eprintln!("[hiconc] building {pregrow}-row SQL table…");
        build_sql_table(&sql_engine, pregrow, 96, false);

        let raw_dir = tempdir().unwrap();
        let raw_engine = Arc::new(Engine::open(raw_dir.path(), 0).unwrap());
        raw_engine.set_deferred_sync(true);
        eprintln!("[hiconc] building {pregrow}-row raw heap…");
        {
            let payload = [0xABu8; 96];
            let mut x = raw_engine.begin().unwrap();
            for i in 0..pregrow {
                raw_engine.insert(x, &payload).unwrap();
                if (i + 1) % 20_000 == 0 {
                    raw_engine.commit(x).unwrap();
                    x = raw_engine.begin().unwrap();
                }
            }
            raw_engine.commit(x).unwrap();
        }

        // Postgres column (matched-durability lens), pre-grown to the same size.
        if let Some(u) = pg_on() {
            eprintln!("[hiconc] building {pregrow}-row Postgres table…");
            pg_build_table(u, "hc", pregrow, false);
        }

        println!(
            "  {:>7}  {:>20}  {:>20}  {:>20}",
            "writers", "unidb_sql", "unidb_raw", "postgres"
        );
        let (mut sql_base, mut raw_base, mut pg_base) = (0.0f64, 0.0f64, 0.0f64);
        let mut id_base = 1_000_000_000i64;
        for &n in &writers {
            let sql = measure_sql_writers(&sql_engine, n, per, id_base);
            let raw = measure_raw_writers(&raw_engine, n, per);
            let pg = pg_on().and_then(|u| pg_measure_table(u, "hc", n, per, id_base));
            id_base += (n * per) as i64;
            if n == 1 {
                sql_base = sql;
                raw_base = raw;
                pg_base = pg.unwrap_or(0.0);
            }
            let cell = |v: f64, base: f64| format!("{v:>10.0} ({:>4.2}x)", v / base);
            let pg_cell = match pg {
                Some(v) => cell(v, pg_base),
                None => format!("{:>18}", "n/a"),
            };
            println!(
                "  {:>7}  {}  {}  {}",
                n,
                cell(sql, sql_base),
                cell(raw, raw_base),
                pg_cell
            );
        }
        drop(sql_engine);
        drop(raw_engine);
    }

    // ---- Table B: SQL size-independence at 8 writers (unidb_sql vs Postgres) ----
    if run("b") {
        println!("\n=== B. Concurrent-write throughput vs table size (8 writers, commits/sec) ===");
        println!(
            "  {:>12}  {:>16}  {:>16}",
            "table_rows", "unidb_sql", "postgres"
        );
        let sizes: Vec<u64> = std::env::var("HICONC_SIZES")
            .ok()
            .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
            .unwrap_or_else(|| vec![100_000, 500_000, 1_000_000, 2_000_000]);
        for &sz in &sizes {
            let dir = tempdir().unwrap();
            let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
            engine.set_deferred_sync(true);
            eprintln!("[hiconc] size-sweep building {sz}-row unidb table…");
            build_sql_table(&engine, sz, 96, false);
            let tput = measure_sql_writers(&engine, 8, per, 1_000_000_000);
            let pg = pg_on().and_then(|u| {
                eprintln!("[hiconc] size-sweep building {sz}-row Postgres table…");
                pg_build_table(u, "hb", sz, false)?;
                pg_measure_table(u, "hb", 8, per, 1_000_000_000)
            });
            let pg_cell = match pg {
                Some(v) => format!("{v:>16.0}"),
                None => format!("{:>16}", "n/a"),
            };
            println!("  {:>12}  {:>16.0}  {}", sz, tput, pg_cell);
        }
    }

    // ---- Table C: indexed vs unindexed (Q2 — is B-tree latch the constraint?) ----
    if run("c") {
        println!(
            "\n=== C. Indexed vs unindexed insert (does index maintenance change scaling?) ==="
        );
        println!(
            "  {:>10}  {:>7}  {:>16}  {:>16}",
            "schema", "writers", "unidb_sql", "postgres"
        );
        for &indexed in &[false, true] {
            let dir = tempdir().unwrap();
            let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
            engine.set_deferred_sync(true);
            let label = if indexed { "indexed" } else { "no-index" };
            eprintln!("[hiconc] table C building {idx_pregrow}-row {label} unidb table…");
            build_sql_table(&engine, idx_pregrow, 96, indexed);
            let pg_name = if indexed { "hc_idx" } else { "hc_noidx" };
            if let Some(u) = pg_on() {
                eprintln!("[hiconc] table C building {idx_pregrow}-row {label} Postgres table…");
                pg_build_table(u, pg_name, idx_pregrow, indexed);
            }
            let (mut base, mut pg_base) = (0.0f64, 0.0f64);
            let mut id_base = 2_000_000_000i64;
            for &n in &[1usize, 8] {
                let tput = measure_sql_writers(&engine, n, per, id_base);
                let pg = pg_on().and_then(|u| pg_measure_table(u, pg_name, n, per, id_base));
                id_base += (n * per) as i64;
                if n == 1 {
                    base = tput;
                    pg_base = pg.unwrap_or(0.0);
                }
                let pg_cell = match pg {
                    Some(v) => format!("{v:>10.0} ({:>4.2}x)", v / pg_base),
                    None => format!("{:>16}", "n/a"),
                };
                println!(
                    "  {:>10}  {:>7}  {:>10.0} ({:>4.2}x)  {}",
                    label,
                    n,
                    tput,
                    tput / base,
                    pg_cell
                );
            }
        }
    }

    // Restore the server-wide durability lens we set at the top.
    if let Some(u) = url.as_deref() {
        pg_reset_lens(u);
    }
    println!("\n########## END HIGH-SCALE CONCURRENCY EXPERIMENT ##########");
}

fn main() {
    // `UNIDB_BENCH` selects a subset so a single section can be re-run quickly
    // (e.g. the durable-FSM B-accept re-runs just B3/B4 before vs after without
    // paying for the full criterion ladders): "b3", "b4", or "b3b4". Unset =
    // the full suite.
    let only = std::env::var("UNIDB_BENCH").unwrap_or_default();
    match only.as_str() {
        "b3" => {
            bench_pg_concurrency();
            return;
        }
        "b4" => {
            bench_size_sweep();
            return;
        }
        "b3b4" => {
            bench_pg_concurrency();
            bench_size_sweep();
            return;
        }
        "fsm" => {
            bench_fsm_scale();
            return;
        }
        "cscale" => {
            bench_conc_scale();
            return;
        }
        "hiconc" => {
            bench_hiconc();
            return;
        }
        _ => {}
    }

    // Criterion-measured groups: the ladder (existing) + B1 pg ladder + B2 CRUD.
    let mut criterion = Criterion::default().configure_from_args();
    bench_ladder(&mut criterion);
    bench_pg_ladder(&mut criterion);
    bench_pg_crud(&mut criterion);
    criterion.final_summary();

    // Manual throughput sweeps that print their own tables: B3 concurrency, B4
    // size sweep. Skipped-Postgres-column handling is internal.
    bench_pg_concurrency();
    bench_size_sweep();
}
