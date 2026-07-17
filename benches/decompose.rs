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
use unidb::{Engine, SqlResult};

/// Every bench engine gets a generously-sized buffer pool, not the library's
/// small default (`DEFAULT_POOL_CAPACITY`, 65536 frames / 512 MiB). That
/// default is deliberately modest -- `BufferPool::open` allocates the frame
/// table eagerly, so a huge default would tax every `Engine::open()` call in
/// the codebase, including the ~50 workspace test files (measured: 530us/open
/// at 1M frames vs 35us/open at 65536 -- see PROGRESS.md "Default buffer-pool
/// capacity raised"). A benchmark that deliberately creates multi-million-row
/// tables is exactly the case that tradeoff exists to protect *other* callers
/// from, not itself, so every bench engine here opts into a much larger pool
/// via `Engine::open_with_pool_capacity` instead of the library default.
/// Without this, large-`MM_SIZES`/`MM_FK_ORDERS` runs silently hit
/// `BufferPoolFull` and report misleadingly slow numbers that reflect pool
/// exhaustion, not the engine's real throughput -- the same pathology found
/// and fixed for the `unidb-studio` demo. Override with `UNIDB_BUFFER_POOL_PAGES`
/// (same name as the engine's own env var); defaults to 2,000,000 frames
/// (~16 GiB working-set ceiling, ~48 MiB of actual frame-table bookkeeping)
/// if unset.
fn bench_engine_open(dir: &std::path::Path) -> Engine {
    let capacity = env_u64("UNIDB_BUFFER_POOL_PAGES", 2_000_000) as usize;
    Engine::open_with_pool_capacity(dir, 0, capacity).unwrap()
}

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
    let engine = bench_engine_open(dir.path());
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

/// `connect_timeout` (seconds) applied to every Postgres connection this bench
/// opens. Override via `PG_CONNECT_TIMEOUT_SECS`.
fn pg_connect_timeout() -> Duration {
    Duration::from_secs(env_u64("PG_CONNECT_TIMEOUT_SECS", 10))
}

/// The one place this bench opens a Postgres connection — always through a
/// parsed `Config` with an explicit `connect_timeout` (see above). Without it,
/// `tokio_postgres`'s underlying `TcpStream::connect` has no timeout of its
/// own: a `PG_URL` that is unreachable (wrong host/port, firewalled, container
/// still starting) makes the connect call block on the OS TCP retry ceiling —
/// empirically 2+ minutes per attempt on this host — with zero output, and
/// this bench dials Postgres from 20+ call sites across the ladder/CRUD/bulk/FK
/// suites, so a single unreachable `PG_URL` used to stall the entire report
/// generation indefinitely instead of failing fast with a clear error. Same
/// `Result<Client, _>` shape as `Client::connect` so every call site is a
/// drop-in replacement.
fn pg_dial(url: &str) -> std::result::Result<Client, Box<dyn std::error::Error + Send + Sync>> {
    let mut cfg: postgres::Config = url.parse()?;
    cfg.connect_timeout(pg_connect_timeout());
    Ok(cfg.connect(NoTls)?)
}

fn pg_connect(url: &str) -> Option<Client> {
    match pg_dial(url) {
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
    let mut admin = pg_connect(url)?;
    let want = if durable {
        // Portable durable lens: pick the strongest flush-to-platter method THIS
        // server actually offers, read from its own `pg_settings.enumvals`, rather
        // than assuming the client's platform. `fsync_writethrough` (F_FULLFSYNC)
        // exists only on macOS Postgres; a Linux server (e.g. in a Docker
        // container) has no such value, and the old hard-coded string made
        // `ALTER SYSTEM` error → the entire Postgres comparison was silently
        // skipped on Linux. See `pg_durable_sync_method`.
        pg_durable_sync_method(&mut admin)
    } else {
        "open_datasync".to_string()
    };
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

/// The strongest flush-to-platter `wal_sync_method` this Postgres server
/// supports, chosen from the server's own `pg_settings.enumvals` so it is correct
/// regardless of the OS Postgres runs on. On macOS that is `fsync_writethrough`
/// (issues `F_FULLFSYNC`, matching unidb's macOS commit sync via Rust std
/// `File::sync_all`); on Linux it is `fsync` (Linux `fsync()` already flushes to
/// the device, matching unidb's Linux commit sync). This is what makes the same
/// comparison fair whether Postgres runs natively on macOS or in a Linux
/// container. Falls back to `fsync` (present everywhere) if the enum can't be read.
fn pg_durable_sync_method(admin: &mut Client) -> String {
    let vals: Vec<String> = admin
        .query_one(
            "SELECT enumvals FROM pg_settings WHERE name = 'wal_sync_method'",
            &[],
        )
        .ok()
        .and_then(|r| r.try_get::<usize, Vec<String>>(0).ok())
        .unwrap_or_default();
    for cand in ["fsync_writethrough", "fsync"] {
        if vals.iter().any(|v| v == cand) {
            return cand.to_string();
        }
    }
    "fsync".to_string()
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
    let engine = bench_engine_open(dir);
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
    let engine = Arc::new(bench_engine_open(dir.path()));
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
    let engine = Arc::new(bench_engine_open(dir.path()));
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
            let mut client = pg_dial(&url).unwrap();
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
    let engine = bench_engine_open(dir.path());
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
    let engine = bench_engine_open(dir.path());
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
    let engine = Arc::new(bench_engine_open(dir.path()));
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
            let mut client = pg_dial(&url).unwrap();
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
        let sql_engine = Arc::new(bench_engine_open(sql_dir.path()));
        sql_engine.set_deferred_sync(true);
        eprintln!("[hiconc] building {pregrow}-row SQL table…");
        build_sql_table(&sql_engine, pregrow, 96, false);

        let raw_dir = tempdir().unwrap();
        let raw_engine = Arc::new(bench_engine_open(raw_dir.path()));
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
            let engine = Arc::new(bench_engine_open(dir.path()));
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
            let engine = Arc::new(bench_engine_open(dir.path()));
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

// ============ Multi-model at-scale report (UNIDB_BENCH=mmreport) =============
//
// A self-contained report generator: the W0→W4 decomposition ladder pre-grown to
// increasing table sizes, so the question "does the ~1.2× multi-model tax hold at
// scale?" is answered by measurement, not reasoning. Emits a complete markdown
// body (definitions + tables + caveats); `scripts/multi_model_report.sh` wraps it
// with a machine/date/RSS header and writes the file. Driven by `scripts/`, but a
// bare `UNIDB_BENCH=mmreport` run prints the same body to stdout.

/// One ladder point: a fresh engine carrying `rung`'s full schema, pre-grown to
/// `size` rows at that rung (batched commits), then the marginal **ms per durable
/// commit** measured over `sample` further single-row transactions AT that size.
/// Every commit is one group-coalesced fsync (deferred-sync), so the number is the
/// per-commit cost the async-derivation design targets — not a bulk-load average.
fn mm_ladder_point(rung: u8, size: u64, sample: u64) -> f64 {
    let dir = tempdir().unwrap();
    let engine = bench_engine_open(dir.path());
    engine.set_deferred_sync(true); // group commit: one fsync per commit
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
        engine.enable_events("t").unwrap(); // inserts now auto-capture events
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
    let do_row = |xid: u64, i: u64| {
        let mut params = vec![Literal::Int(i as i64), Literal::Text(format!("b{i}"))];
        if rung >= 2 {
            params.push(Literal::Vector(embedding(i)));
        }
        engine.execute_prepared(xid, &ins, &params).unwrap();
        if rung >= 3 {
            engine
                .create_edge(xid, i as i64, (i as i64) + 1, "rel", "{}")
                .unwrap();
        }
    };
    // Pre-grow (batched — not timed).
    let mut x = engine.begin().unwrap();
    for i in 0..size {
        do_row(x, i);
        if (i + 1) % 2_000 == 0 {
            engine.commit(x).unwrap();
            x = engine.begin().unwrap();
        }
    }
    engine.commit(x).unwrap();
    // Measure marginal per-commit cost at this size.
    let start = Instant::now();
    for j in 0..sample {
        let xid = engine.begin().unwrap();
        do_row(xid, size + j);
        engine.commit(xid).unwrap();
    }
    start.elapsed().as_secs_f64() * 1000.0 / sample as f64
}

// ============ Table 3 CRUD + Table 4 at-scale helpers (mmreport) ============
// A richer mmreport (per-request): Table 3 becomes a full CRUD stress suite
// (insert / filtered+grouped select / bulk update / selected+full delete)
// comparing unidb (SQL path) vs Postgres relational at matched fsync, printing
// how many records each op touches; Table 4 becomes a MEASURED unidb-multi-model
// (W4) vs Postgres-relational throughput sweep across tx counts (to millions).
// Every measured phase is bracketed by `phased(..)` so an external docker-stats
// sampler can attribute per-phase CPU/memory to each container.

const CRUD_GROUPS: i64 = 100;

/// Emit a phase boundary marker (to stderr and, if `MM_PHASES` is set, appended
/// to that file as `name,edge,unix_ms`). The host-side docker-stats sampler
/// correlates these windows to per-container CPU/mem samples.
fn phase_mark(name: &str, edge: &str) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    eprintln!("[[PHASE {edge} {name} {ms}]]");
    if let Ok(path) = std::env::var("MM_PHASES") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{name},{edge},{ms}");
        }
    }
}

/// Run `f` bracketed by start/end phase markers; returns f's value.
fn phased<T>(name: &str, f: impl FnOnce() -> T) -> T {
    phase_mark(name, "start");
    let r = f();
    phase_mark(name, "end");
    r
}

/// records/sec for (count, secs), guarding divide-by-zero.
fn rps(count: u64, secs: f64) -> f64 {
    if secs > 0.0 {
        count as f64 / secs
    } else {
        0.0
    }
}

/// Winner + margin for a unidb-vs-Postgres throughput pair (records/sec):
/// whoever is faster, and by what percent. Near-equal reads as parity; a missing
/// side (0 rec/s — e.g. Postgres skipped) is called out rather than shown as a win.
fn winner_remark(uu: f64, pp: f64) -> String {
    if uu <= 0.0 && pp <= 0.0 {
        return "—".to_string();
    }
    if pp <= 0.0 {
        return "unidb (PG n/a)".to_string();
    }
    if uu <= 0.0 {
        return "postgres (unidb n/a)".to_string();
    }
    let (name, fast, slow) = if uu >= pp {
        ("unidb", uu, pp)
    } else {
        ("postgres", pp, uu)
    };
    let pct = (fast / slow - 1.0) * 100.0;
    if pct < 1.0 {
        "≈ parity".to_string()
    } else {
        format!("**{name}** +{pct:.0}%")
    }
}

/// C1 measurement: run a unidb CRUD op bracketed by cumulative WAL-bytes and
/// rows-decoded reads, returning `(records, secs, wal_bytes_delta,
/// rows_decoded_delta)`. `wal_total_bytes_appended` survives auto-checkpoint
/// truncation, so the delta is the true WAL volume the op produced.
///
/// Item 59 Fix 1: enables `COLS_DECODED` diagnostics on first call (the flag
/// is process-global; enabling it once is sufficient for the whole bench run).
fn measured_unidb(
    engine: &Arc<Engine>,
    f: impl FnOnce() -> (u64, f64),
) -> (u64, f64, u64, u64, u64) {
    // Enable the COLS_DECODED counter (gated by DIAGNOSTICS_ENABLED by default
    // to avoid atomic overhead on the hot path in non-bench contexts).
    Engine::enable_diagnostics();
    let wal0 = engine.wal_total_bytes_appended();
    let dec0 = Engine::rows_decoded_total();
    let cols0 = Engine::cols_decoded_total();
    let (count, secs) = f();
    let wal = engine.wal_total_bytes_appended().saturating_sub(wal0);
    let dec = Engine::rows_decoded_total().saturating_sub(dec0);
    let cols = Engine::cols_decoded_total().saturating_sub(cols0);
    (count, secs, wal, dec, cols)
}

/// Print one CRUD row plus the proof columns: unidb WAL bytes/row (C1),
/// rows-decoded/row (C1), and columns-materialized/row (C1′, the Phase-B decode-
/// pushdown proof). `u` carries the measured deltas from [`measured_unidb`].
fn crud_row_c1(op: &str, u: (u64, f64, u64, u64, u64), p: (u64, f64)) {
    let (uu, pp) = (rps(u.0, u.1), rps(p.0, p.1));
    let ratio = if pp > 0.0 { uu / pp } else { 0.0 };
    let per = u.0.max(1);
    let wal_per_row = u.2 as f64 / per as f64;
    let dec_per_row = u.3 as f64 / per as f64;
    let cols_per_row = u.4 as f64 / per as f64;
    println!(
        "| {op} | {} | {:.0} | {:.0} | {:.2}× | {} | {:.0} | {:.2} | {:.2} |",
        u.0.max(p.0),
        uu,
        pp,
        ratio,
        winner_remark(uu, pp),
        wal_per_row,
        dec_per_row,
        cols_per_row,
    );
}

fn sql_rows_returned(res: &[SqlResult]) -> u64 {
    res.iter()
        .map(|r| match r {
            SqlResult::Rows { rows, .. } => rows.len(),
            _ => 0,
        })
        .sum::<usize>() as u64
}

fn sql_affected(res: &[SqlResult]) -> u64 {
    res.iter()
        .map(|r| match r {
            SqlResult::Updated { count }
            | SqlResult::Deleted { count }
            | SqlResult::Inserted { count }
            | SqlResult::Truncated { count } => *count as u64,
            _ => 0u64,
        })
        .sum()
}

// ---- unidb CRUD (SQL path) ----

/// Build unidb CRUD table `t (id, k, g, body)` with `rows` rows: k = unique
/// range/filter key, g = k % CRUD_GROUPS grouping key; btree on k.
fn sql_build_crud(engine: &Arc<Engine>, rows: u64) {
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
                    Literal::Int(i as i64),
                    Literal::Int(i as i64),
                    Literal::Int((i as i64) % CRUD_GROUPS),
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
}

/// Bulk INSERT: `n` new rows (k in [base, base+n)), each its own durable commit.
fn sql_crud_insert(engine: &Arc<Engine>, n: u64, base: i64) -> (u64, f64) {
    let ins = engine
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();
    let start = Instant::now();
    for i in 0..n {
        let id = base + i as i64;
        let xid = engine.begin().unwrap();
        engine
            .execute_prepared(
                xid,
                &ins,
                &[
                    Literal::Int(id),
                    Literal::Int(id),
                    Literal::Int(id % CRUD_GROUPS),
                    Literal::Text(format!("b{id}")),
                ],
            )
            .unwrap();
        engine.commit(xid).unwrap();
    }
    (n, start.elapsed().as_secs_f64())
}

fn sql_crud_select_filtered(engine: &Arc<Engine>, lo: i64, hi: i64) -> (u64, f64) {
    let x = engine.begin().unwrap();
    let start = Instant::now();
    let res = engine
        .execute_sql(
            x,
            &format!("SELECT id, body FROM t WHERE k >= {lo} AND k < {hi}"),
        )
        .unwrap();
    let secs = start.elapsed().as_secs_f64();
    engine.commit(x).unwrap();
    (sql_rows_returned(&res), secs)
}

fn sql_crud_select_grouped(engine: &Arc<Engine>, scanned: u64) -> (u64, f64) {
    let x = engine.begin().unwrap();
    let start = Instant::now();
    let _res = engine
        .execute_sql(x, "SELECT g, COUNT(*) FROM t GROUP BY g")
        .unwrap();
    let secs = start.elapsed().as_secs_f64();
    engine.commit(x).unwrap();
    (scanned, secs) // throughput = rows scanned / sec
}

/// Unfiltered `SELECT COUNT(*)` — the B1 count-visible-slots fast path (decodes
/// nothing). Throughput = rows counted / sec.
fn sql_crud_count_all(engine: &Arc<Engine>, scanned: u64) -> (u64, f64) {
    let x = engine.begin().unwrap();
    let start = Instant::now();
    let _res = engine.execute_sql(x, "SELECT COUNT(*) FROM t").unwrap();
    let secs = start.elapsed().as_secs_f64();
    engine.commit(x).unwrap();
    (scanned, secs)
}

fn sql_crud_update_bulk(engine: &Arc<Engine>, hi: i64) -> (u64, f64) {
    let x = engine.begin().unwrap();
    let start = Instant::now();
    let res = engine
        .execute_sql(x, &format!("UPDATE t SET body = 'updated' WHERE k < {hi}"))
        .unwrap();
    engine.commit(x).unwrap();
    (sql_affected(&res), start.elapsed().as_secs_f64())
}

fn sql_crud_delete_selected(engine: &Arc<Engine>, lo: i64) -> (u64, f64) {
    let x = engine.begin().unwrap();
    let start = Instant::now();
    let res = engine
        .execute_sql(x, &format!("DELETE FROM t WHERE k >= {lo}"))
        .unwrap();
    engine.commit(x).unwrap();
    (sql_affected(&res), start.elapsed().as_secs_f64())
}

fn sql_crud_delete_all(engine: &Arc<Engine>) -> (u64, f64) {
    let x = engine.begin().unwrap();
    let start = Instant::now();
    let res = engine.execute_sql(x, "DELETE FROM t").unwrap();
    engine.commit(x).unwrap();
    (sql_affected(&res), start.elapsed().as_secs_f64())
}

// ---- Postgres CRUD (relational) ----

fn pg_build_crud(url: &str, rows: u64) -> Option<()> {
    let mut c = pg_connect(url)?;
    c.batch_execute(
        "DROP TABLE IF EXISTS t; CREATE TABLE t (id BIGINT, k BIGINT, g BIGINT, body TEXT)",
    )
    .ok()?;
    c.batch_execute("CREATE INDEX t_k ON t (k)").ok()?;
    if rows > 0 {
        c.execute(
            "INSERT INTO t (id, k, g, body) \
             SELECT s, s, s % $2, 'b' || s FROM generate_series(0, $1::bigint - 1) s",
            &[&(rows as i64), &CRUD_GROUPS],
        )
        .ok()?;
    }
    Some(())
}

fn pg_crud_insert(url: &str, n: u64, base: i64) -> (u64, f64) {
    let mut c = pg_dial(url).unwrap();
    let ins = c
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();
    let start = Instant::now();
    for i in 0..n {
        let id = base + i as i64;
        c.execute(&ins, &[&id, &id, &(id % CRUD_GROUPS), &format!("b{id}")])
            .unwrap();
    }
    (n, start.elapsed().as_secs_f64())
}

fn pg_crud_select_filtered(url: &str, lo: i64, hi: i64) -> (u64, f64) {
    let mut c = pg_dial(url).unwrap();
    // Cap to 2 workers so SELECT ratios don't vary with core count across environments
    // (on an 18-core machine PG would otherwise use far more workers than on a 4-core).
    c.batch_execute("SET max_parallel_workers_per_gather = 2")
        .unwrap();
    let start = Instant::now();
    let rows = c
        .query(
            "SELECT id, body FROM t WHERE k >= $1 AND k < $2",
            &[&lo, &hi],
        )
        .unwrap();
    (rows.len() as u64, start.elapsed().as_secs_f64())
}

fn pg_crud_select_grouped(url: &str, scanned: u64) -> (u64, f64) {
    let mut c = pg_dial(url).unwrap();
    c.batch_execute("SET max_parallel_workers_per_gather = 2")
        .unwrap();
    let start = Instant::now();
    let _rows = c
        .query("SELECT g, COUNT(*) FROM t GROUP BY g", &[])
        .unwrap();
    (scanned, start.elapsed().as_secs_f64())
}

fn pg_crud_count_all(url: &str, scanned: u64) -> (u64, f64) {
    let mut c = pg_dial(url).unwrap();
    c.batch_execute("SET max_parallel_workers_per_gather = 2")
        .unwrap();
    let start = Instant::now();
    let _rows = c.query("SELECT COUNT(*) FROM t", &[]).unwrap();
    (scanned, start.elapsed().as_secs_f64())
}

fn pg_crud_update_bulk(url: &str, hi: i64) -> (u64, f64) {
    let mut c = pg_dial(url).unwrap();
    let start = Instant::now();
    let n = c
        .execute("UPDATE t SET body = 'updated' WHERE k < $1", &[&hi])
        .unwrap();
    (n, start.elapsed().as_secs_f64())
}

fn pg_crud_delete_selected(url: &str, lo: i64) -> (u64, f64) {
    let mut c = pg_dial(url).unwrap();
    let start = Instant::now();
    let n = c.execute("DELETE FROM t WHERE k >= $1", &[&lo]).unwrap();
    (n, start.elapsed().as_secs_f64())
}

fn pg_crud_delete_all(url: &str) -> (u64, f64) {
    let mut c = pg_dial(url).unwrap();
    let start = Instant::now();
    let n = c.execute("DELETE FROM t", &[]).unwrap();
    (n, start.elapsed().as_secs_f64())
}

// ---- Table 3.1: bulk stress (insert + full-scan select at scale) ----
// Fresh table per size; `n` rows bulk-loaded via batched prepared single-row
// inserts (one durable commit per 5k rows) — the SAME method on both engines so
// the insert comparison is apples-to-apples — then a full-table `SELECT COUNT(*)`
// scan is timed. Throughput is records/sec (insert rows, then rows scanned).

const BULK_COMMIT_BATCH: u64 = 5_000;

/// unidb bulk insert: build a fresh `bt (id INT PRIMARY KEY, k INT, body TEXT)`
/// and insert `n` rows, committing every `BULK_COMMIT_BATCH`. The PRIMARY KEY
/// exercises `enforce_unique` (item 35 fix — index-backed, not a heap scan).
/// Returns (rows, secs).
fn sql_bulk_insert(engine: &Arc<Engine>, n: u64) -> (u64, f64) {
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE bt (id INT PRIMARY KEY, k INT, body TEXT)")
        .unwrap();
    engine
        .execute_sql(x, "CREATE INDEX bt_k ON bt USING BTREE (k)")
        .unwrap();
    engine.commit(x).unwrap();
    let ins = engine
        .prepare("INSERT INTO bt (id, k, body) VALUES ($1, $2, $3)")
        .unwrap();
    let start = Instant::now();
    let mut x = engine.begin().unwrap();
    for i in 0..n {
        engine
            .execute_prepared(
                x,
                &ins,
                &[
                    Literal::Int(i as i64),
                    Literal::Int(i as i64),
                    Literal::Text(format!("b{i}")),
                ],
            )
            .unwrap();
        if (i + 1).is_multiple_of(BULK_COMMIT_BATCH) {
            engine.commit(x).unwrap();
            x = engine.begin().unwrap();
        }
    }
    engine.commit(x).unwrap();
    (n, start.elapsed().as_secs_f64())
}

/// unidb full **heap** scan of `bt`: the predicate is on the non-indexed `body`
/// column (matches all rows), so neither engine can serve it index-only — this
/// measures real scan throughput, not a count optimizer. Throughput = rows/sec.
fn sql_bulk_select(engine: &Arc<Engine>, n: u64) -> (u64, f64) {
    let x = engine.begin().unwrap();
    let start = Instant::now();
    let _res = engine
        .execute_sql(x, "SELECT COUNT(*) FROM bt WHERE body <> 'x'")
        .unwrap();
    let secs = start.elapsed().as_secs_f64();
    engine.commit(x).unwrap();
    (n, secs)
}

/// Postgres bulk insert into a fresh `bt`, matched method: batched prepared
/// single-row inserts, one commit per `BULK_COMMIT_BATCH`. Returns (rows, secs).
fn pg_bulk_insert(url: &str, n: u64) -> (u64, f64) {
    let mut c = pg_dial(url).unwrap();
    c.batch_execute(
        "DROP TABLE IF EXISTS bt; \
         CREATE TABLE bt (id BIGINT, k BIGINT, body TEXT); \
         CREATE INDEX bt_k ON bt (k)",
    )
    .unwrap();
    let ins = c
        .prepare("INSERT INTO bt (id, k, body) VALUES ($1, $2, $3)")
        .unwrap();
    let start = Instant::now();
    let mut tx = c.transaction().unwrap();
    for i in 0..n as i64 {
        tx.execute(&ins, &[&i, &i, &format!("b{i}")]).unwrap();
        if (i as u64 + 1).is_multiple_of(BULK_COMMIT_BATCH) {
            tx.commit().unwrap();
            tx = c.transaction().unwrap();
        }
    }
    tx.commit().unwrap();
    (n, start.elapsed().as_secs_f64())
}

/// Postgres full **heap** scan of `bt`, matched to `sql_bulk_select`: the
/// non-indexed `body` predicate forces a seq scan (no index-only shortcut).
/// Throughput = rows/sec.
fn pg_bulk_select(url: &str, n: u64) -> (u64, f64) {
    let mut c = pg_dial(url).unwrap();
    let start = Instant::now();
    let _rows = c
        .query("SELECT COUNT(*) FROM bt WHERE body <> 'x'", &[])
        .unwrap();
    (n, start.elapsed().as_secs_f64())
}

// ---- Table 4: at-scale throughput helpers ----

/// unidb multi-model (W4) commits/sec: `n` transactions, each inserting a row +
/// VECTOR(128) + graph edge + event, one durable group-commit each.
fn unidb_w4_throughput(n: u64) -> f64 {
    let dir = tempdir().unwrap();
    let engine = bench_engine_open(dir.path());
    engine.set_deferred_sync(true);
    let sx = engine.begin().unwrap();
    engine
        .execute_sql(
            sx,
            "CREATE TABLE t (id INT, body TEXT, embedding VECTOR(128))",
        )
        .unwrap();
    engine
        .execute_sql(sx, "CREATE INDEX ib ON t USING BTREE (id)")
        .unwrap();
    engine
        .execute_sql(sx, "CREATE INDEX iv ON t USING HNSW (embedding)")
        .unwrap();
    engine.commit(sx).unwrap();
    engine.enable_events("t").unwrap();
    let ins = engine
        .prepare("INSERT INTO t (id, body, embedding) VALUES ($1, $2, $3)")
        .unwrap();
    let start = Instant::now();
    for i in 0..n {
        let xid = engine.begin().unwrap();
        engine
            .execute_prepared(
                xid,
                &ins,
                &[
                    Literal::Int(i as i64),
                    Literal::Text(format!("body-{i}")),
                    Literal::Vector(embedding(i)),
                ],
            )
            .unwrap();
        engine
            .create_edge(xid, i as i64, (i as i64) + 1, "rel", "{}")
            .unwrap();
        engine.commit(xid).unwrap();
    }
    rps(n, start.elapsed().as_secs_f64())
}

/// Postgres relational commits/sec: `n` single-row durable INSERTs.
fn pg_relational_throughput(url: &str, n: u64) -> f64 {
    let mut c = pg_dial(url).unwrap();
    c.batch_execute("DROP TABLE IF EXISTS t4; CREATE TABLE t4 (id BIGINT PRIMARY KEY, body TEXT)")
        .unwrap();
    let ins = c
        .prepare("INSERT INTO t4 (id, body) VALUES ($1, $2)")
        .unwrap();
    let start = Instant::now();
    for i in 0..n as i64 {
        c.execute(&ins, &[&i, &format!("body-{i}")]).unwrap();
    }
    rps(n, start.elapsed().as_secs_f64())
}

/// Format an f32 slice as a pgvector text literal `[x,y,z]` (cast to `::vector`
/// on insert). pgvector has no native Rust `postgres` type, so text-with-cast is
/// the standard bind path.
fn pg_vector_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 6 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

/// The **replaced stack** (CLAUDE.md §6): the *same* four model-writes unidb's W4
/// folds into ONE atomic commit, executed here as **four independent durable
/// operations with no shared transaction** — Postgres (row) + a vector store
/// (pgvector + HNSW) + a graph store (adjacency) + a queue (outbox). Each of the
/// four is its **own connection** and its **own auto-committed statement**, so
/// each incurs its own `fsync` and the four cannot group-commit-coalesce — this
/// *is* the dual-write tax: 4 fsyncs / 4 round-trips per record, and (unlike
/// unidb) **no cross-system atomicity** — a crash mid-sequence leaves a torn
/// record. Returns commits/sec, or `None` if pgvector is unavailable (column then
/// skipped, exactly like `PG_URL` unset). Conservative floor: real Neo4j/Kafka are
/// heavier than PG tables, so the true tax (and unidb's win) is larger.
fn pg_replaced_stack_throughput(url: &str, n: u64) -> Option<f64> {
    // Four "systems", four connections — no shared transaction between them.
    let mut rel = pg_connect(url)?; // relational
    let mut vec = pg_connect(url)?; // vector store
    let mut grf = pg_connect(url)?; // graph store
    let mut que = pg_connect(url)?; // queue / outbox

    // pgvector availability gate — skip the whole replaced-stack column if absent
    // (needs the `pgvector/pgvector` image, not stock `postgres`).
    if vec
        .batch_execute("CREATE EXTENSION IF NOT EXISTS vector")
        .is_err()
    {
        eprintln!("  [pg] pgvector extension unavailable — replaced-stack column skipped");
        return None;
    }
    rel.batch_execute(
        "DROP TABLE IF EXISTS rs_rel; CREATE TABLE rs_rel (id BIGINT PRIMARY KEY, body TEXT)",
    )
    .ok()?;
    // HNSW to mirror unidb's W4 vector index (per-insert ANN maintenance is the
    // fair cost). Index created before inserts so each insert maintains the graph.
    vec.batch_execute(&format!(
        "DROP TABLE IF EXISTS rs_vec; CREATE TABLE rs_vec (id BIGINT PRIMARY KEY, embedding vector({DIM})); \
         CREATE INDEX rs_vec_ann ON rs_vec USING hnsw (embedding vector_cosine_ops)"
    ))
    .ok()?;
    grf.batch_execute(
        "DROP TABLE IF EXISTS rs_edge; \
         CREATE TABLE rs_edge (from_id BIGINT, to_id BIGINT, edge_type TEXT); \
         CREATE INDEX rs_edge_from ON rs_edge (from_id)",
    )
    .ok()?;
    que.batch_execute(
        "DROP TABLE IF EXISTS rs_out; \
         CREATE TABLE rs_out (seq BIGSERIAL PRIMARY KEY, kind TEXT, payload TEXT)",
    )
    .ok()?;

    let ins_rel = rel
        .prepare("INSERT INTO rs_rel (id, body) VALUES ($1, $2)")
        .ok()?;
    // `$2::text::vector`, not `$2::vector`: the latter makes Postgres infer `$2`
    // as type `vector` (no Rust `ToSql` for it → WrongType panic). Forcing the
    // param to `text` first lets us bind the pgvector literal as a `String`.
    let ins_vec = vec
        .prepare("INSERT INTO rs_vec (id, embedding) VALUES ($1, $2::text::vector)")
        .ok()?;
    let ins_grf = grf
        .prepare("INSERT INTO rs_edge (from_id, to_id, edge_type) VALUES ($1, $2, $3)")
        .ok()?;
    let ins_que = que
        .prepare("INSERT INTO rs_out (kind, payload) VALUES ($1, $2)")
        .ok()?;

    let start = Instant::now();
    for i in 0..n as i64 {
        // Four separate durable commits — the point of the comparison.
        rel.execute(&ins_rel, &[&i, &format!("body-{i}")]).unwrap();
        let lit = pg_vector_literal(&embedding(i as u64));
        vec.execute(&ins_vec, &[&i, &lit]).unwrap();
        grf.execute(&ins_grf, &[&i, &(i + 1), &"rel"]).unwrap();
        que.execute(&ins_que, &[&"insert", &format!("body-{i}")])
            .unwrap();
    }
    Some(rps(n, start.elapsed().as_secs_f64()))
}

/// The crash-consistency face, replaced-stack side. Because the four writes are
/// four independent commits with no shared transaction, an interruption **after
/// the relational commit** durably keeps the row while the embedding/edge/event
/// never land — a **torn record** that no recovery rolls back (contrast unidb's
/// all-or-nothing: `tests/crash/item16_incomplete_four_model_txn_leaves_zero_orphans`).
/// Returns `Some(true)` if the orphan is observed on a *fresh* connection (i.e.
/// durably, as after a restart). Requires the tables from a prior
/// `pg_replaced_stack_throughput` setup; safe no-op skip if pgvector is absent.
fn pg_stack_torn_record_demo(url: &str) -> Option<bool> {
    let orphan_id: i64 = -777; // outside the 0..n throughput range
    {
        // "System 1" commits the row durably…
        let mut rel = pg_connect(url)?;
        rel.execute(
            "INSERT INTO rs_rel (id, body) VALUES ($1, $2)",
            &[&orphan_id, &"torn-record-demo"],
        )
        .ok()?;
        // …then the process is "interrupted" before systems 2–4 run. (We simply
        // do not issue the other three commits — the honest structural point:
        // there is no transaction spanning them to undo the row.)
    }
    // Fresh connections = "after restart": the row is durably present, but the
    // embedding / edge / event are absent → a torn record.
    let mut c = pg_connect(url)?;
    let row_present: i64 = c
        .query_one("SELECT count(*) FROM rs_rel WHERE id = $1", &[&orphan_id])
        .ok()?
        .get(0);
    let vec_present: i64 = c
        .query_one("SELECT count(*) FROM rs_vec WHERE id = $1", &[&orphan_id])
        .ok()?
        .get(0);
    let edge_present: i64 = c
        .query_one(
            "SELECT count(*) FROM rs_edge WHERE from_id = $1",
            &[&orphan_id],
        )
        .ok()?
        .get(0);
    Some(row_present == 1 && vec_present == 0 && edge_present == 0)
}

/// The durability primitive unidb's commit sync (`File::sync_all`) actually
/// resolves to on the platform this bench is *running* on, so the generated
/// report is internally consistent instead of hard-coding "macOS". Rust std
/// issues `fcntl(F_FULLFSYNC)` on macOS/iOS (true flush-to-platter); everywhere
/// else (Linux, incl. the Docker image) it is a plain `fsync`.
fn unidb_sync_primitive() -> &'static str {
    if cfg!(any(target_os = "macos", target_os = "ios")) {
        "F_FULLFSYNC"
    } else {
        "fsync"
    }
}

// ============ Table 5: PK/FK relational-integrity stress (mmreport) ========
// `customers (id PK)` / `orders (id PK, customer_id FK -> customers.id)`.
// Real row-level FK enforcement on both engines (unidb since item 36,
// 2026-07-14: child INSERT/UPDATE verifies the parent key via the parent's
// implicit unique-index B-tree, O(log n); parent DELETE/UPDATE enforces
// RESTRICT) -- this is a genuinely fair apples-to-apples comparison now,
// not the pre-item-36 "FK is metadata-only" era.
const FK_CUSTOMERS: u64 = 20_000;

fn sql_fk_setup(engine: &Arc<Engine>, customers: u64) {
    let x = engine.begin().unwrap();
    engine
        .execute_sql(x, "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    engine
        .execute_sql(
            x,
            "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT REFERENCES customers(id), amount INT, status TEXT)",
        )
        .unwrap();
    engine.commit(x).unwrap();
    let ins = engine
        .prepare("INSERT INTO customers (id, name) VALUES ($1, $2)")
        .unwrap();
    let mut x = engine.begin().unwrap();
    for i in 0..customers {
        engine
            .execute_prepared(
                x,
                &ins,
                &[
                    Literal::Int(i as i64),
                    Literal::Text(format!("customer{i}")),
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

/// INSERT `n` valid orders, each its own durable commit, `customer_id`
/// cycling through the pre-loaded customer set -- every insert pays a real
/// FK existence check (item 35's implicit unique-index lookup), not a no-op.
fn sql_fk_insert_valid(engine: &Arc<Engine>, n: u64, base: i64, customers: u64) -> (u64, f64) {
    let ins = engine
        .prepare("INSERT INTO orders (id, customer_id, amount, status) VALUES ($1, $2, $3, $4)")
        .unwrap();
    let start = Instant::now();
    for i in 0..n {
        let id = base + i as i64;
        let xid = engine.begin().unwrap();
        engine
            .execute_prepared(
                xid,
                &ins,
                &[
                    Literal::Int(id),
                    Literal::Int(id % customers as i64),
                    Literal::Int(id % 1000),
                    Literal::Text("pending".to_string()),
                ],
            )
            .unwrap();
        engine.commit(xid).unwrap();
    }
    (n, start.elapsed().as_secs_f64())
}

/// Confirm a child INSERT referencing a non-existent parent key is rejected.
fn sql_fk_rejects_invalid(engine: &Arc<Engine>) -> bool {
    let xid = engine.begin().unwrap();
    let res = engine.execute_sql(
        xid,
        "INSERT INTO orders (id, customer_id, amount, status) VALUES (999999999, 888888888, 1, 'x')",
    );
    let rejected = res.is_err();
    let _ = engine.abort(xid);
    rejected
}

/// Confirm deleting a still-referenced parent row is rejected (RESTRICT).
fn sql_fk_restrict_blocks_delete(engine: &Arc<Engine>, referenced_customer_id: i64) -> bool {
    let xid = engine.begin().unwrap();
    let res = engine.execute_sql(
        xid,
        &format!("DELETE FROM customers WHERE id = {referenced_customer_id}"),
    );
    let blocked = res.is_err();
    let _ = engine.abort(xid);
    blocked
}

fn sql_fk_update_bulk(engine: &Arc<Engine>, hi: i64) -> (u64, f64) {
    let x = engine.begin().unwrap();
    let start = Instant::now();
    let res = engine
        .execute_sql(
            x,
            &format!("UPDATE orders SET status = 'shipped' WHERE id < {hi}"),
        )
        .unwrap();
    engine.commit(x).unwrap();
    (sql_affected(&res), start.elapsed().as_secs_f64())
}

/// A realistic FK-linked query: join child to parent, filtered.
fn sql_fk_join_select(engine: &Arc<Engine>) -> (u64, f64) {
    let x = engine.begin().unwrap();
    let start = Instant::now();
    let res = engine
        .execute_sql(
            x,
            "SELECT orders.id, customers.name FROM orders \
             JOIN customers ON orders.customer_id = customers.id \
             WHERE orders.status = 'pending'",
        )
        .unwrap();
    let secs = start.elapsed().as_secs_f64();
    engine.commit(x).unwrap();
    (sql_rows_returned(&res), secs)
}

// ---- Postgres FK (relational integrity) ----

fn pg_fk_setup(url: &str, customers: u64) -> Option<()> {
    let mut c = pg_connect(url)?;
    c.batch_execute(
        "DROP TABLE IF EXISTS orders; DROP TABLE IF EXISTS customers; \
         CREATE TABLE customers (id BIGINT PRIMARY KEY, name TEXT); \
         CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id BIGINT REFERENCES customers(id), amount BIGINT, status TEXT)",
    )
    .ok()?;
    c.execute(
        "INSERT INTO customers (id, name) \
         SELECT s, 'customer' || s FROM generate_series(0, $1::bigint - 1) s",
        &[&(customers as i64)],
    )
    .ok()?;
    Some(())
}

fn pg_fk_insert_valid(url: &str, n: u64, base: i64, customers: u64) -> (u64, f64) {
    let mut c = pg_dial(url).unwrap();
    let ins = c
        .prepare("INSERT INTO orders (id, customer_id, amount, status) VALUES ($1, $2, $3, $4)")
        .unwrap();
    let start = Instant::now();
    for i in 0..n {
        let id = base + i as i64;
        c.execute(
            &ins,
            &[&id, &(id % customers as i64), &(id % 1000), &"pending"],
        )
        .unwrap();
    }
    (n, start.elapsed().as_secs_f64())
}

fn pg_fk_rejects_invalid(url: &str) -> bool {
    let mut c = match pg_dial(url) {
        Ok(c) => c,
        Err(_) => return false,
    };
    c.execute(
        "INSERT INTO orders (id, customer_id, amount, status) VALUES (999999999, 888888888, 1, 'x')",
        &[],
    )
    .is_err()
}

fn pg_fk_restrict_blocks_delete(url: &str, referenced_customer_id: i64) -> bool {
    let mut c = match pg_dial(url) {
        Ok(c) => c,
        Err(_) => return false,
    };
    c.execute(
        "DELETE FROM customers WHERE id = $1",
        &[&referenced_customer_id],
    )
    .is_err()
}

fn pg_fk_update_bulk(url: &str, hi: i64) -> (u64, f64) {
    let mut c = pg_dial(url).unwrap();
    let start = Instant::now();
    let n = c
        .execute("UPDATE orders SET status = 'shipped' WHERE id < $1", &[&hi])
        .unwrap();
    (n, start.elapsed().as_secs_f64())
}

fn pg_fk_join_select(url: &str) -> (u64, f64) {
    let mut c = pg_dial(url).unwrap();
    c.batch_execute("SET max_parallel_workers_per_gather = 2")
        .unwrap();
    let start = Instant::now();
    let rows = c
        .query(
            "SELECT orders.id, customers.name FROM orders \
             JOIN customers ON orders.customer_id = customers.id \
             WHERE orders.status = 'pending'",
            &[],
        )
        .unwrap();
    (rows.len() as u64, start.elapsed().as_secs_f64())
}

fn bench_mm_report() {
    let sync_prim = unidb_sync_primitive();
    let sizes: Vec<u64> = std::env::var("MM_SIZES")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![1_000, 10_000, 100_000]);
    let sample = env_u64("MM_SAMPLE", 200);
    let url = pg_url();

    println!("## What this measures (self-contained — no other docs needed)\n");
    println!(
        "The **W0→W4 decomposition ladder**: one durable transaction, adding one data\n\
         model at a time, so the multi-model write tax is read by subtraction. Every\n\
         commit is one group-coalesced `{sync_prim}` (matched durability). The number\n\
         reported is **marginal ms per durable commit** at each pre-grown table size\n\
         (`MM_SAMPLE={sample}` commits averaged; pre-grow not timed).\n"
    );
    println!("| Rung | What the single transaction does |");
    println!("|------|----------------------------------|");
    println!("| **W0** | plain relational row INSERT (WAL + heap + fsync) |");
    println!("| **W1** | W0 + a B-tree secondary index entry |");
    println!("| **W2** | W1 + a `VECTOR(128)` value + its ANN index (HNSW) |");
    println!("| **W3** | W2 + a graph edge + adjacency-index maintenance |");
    println!("| **W4** | W3 + event-queue capture — **the full four-model commit** |");
    println!(
        "\n`W4−W0` is the total multi-model tax; `W4/W0` is the multiplier. The thesis:\n\
         one shared fsync makes W4 ≈ W0 (not N×), and it should stay that way as rows\n\
         grow. A rising `W4/W0` means per-model index CPU (esp. HNSW) is eroding it.\n"
    );

    // ---- Table 1: cost vs size, and collect for Table 2 ----
    println!("## Table 1 — Multi-model commit cost vs table size (ms/commit)\n");
    println!("| rows | W0 | W1 | W2 | W3 | W4 | W4−W0 | W4/W0 |");
    println!("|-----:|---:|---:|---:|---:|---:|------:|------:|");
    let mut all: Vec<(u64, [f64; 5])> = Vec::new();
    for &size in &sizes {
        eprintln!("[mmreport] ladder at {size} rows…");
        let mut w = [0.0f64; 5];
        for (r, wr) in w.iter_mut().enumerate() {
            *wr = mm_ladder_point(r as u8, size, sample);
        }
        println!(
            "| {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2}× |",
            size,
            w[0],
            w[1],
            w[2],
            w[3],
            w[4],
            w[4] - w[0],
            w[4] / w[0]
        );
        all.push((size, w));
    }

    // ---- Table 2: per-model marginal deltas ----
    println!("\n## Table 2 — Per-model marginal maintenance vs size (ms added per commit)\n");
    println!("| rows | Δ btree (W1−W0) | Δ vector (W2−W1) | Δ edge (W3−W2) | Δ event (W4−W3) |");
    println!("|-----:|----------------:|-----------------:|---------------:|----------------:|");
    for (size, w) in &all {
        println!(
            "| {} | {:+.2} | {:+.2} | {:+.2} | {:+.2} |",
            size,
            w[1] - w[0],
            w[2] - w[1],
            w[3] - w[2],
            w[4] - w[3]
        );
    }
    println!(
        "\n*(small-size deltas are near-noise — every rung sits within a few hundred µs\n\
         of the fsync floor; the vector column is the one expected to separate as rows\n\
         grow, since HNSW insert is O(log n) distance computations.)*\n"
    );

    // ---- Postgres durability lens (shared by Tables 3 & 4) ----
    let pg_method = url.as_deref().and_then(pg_ensure_lens);

    // ---- Table 3: CRUD stress, unidb vs Postgres (relational, matched fsync) ----
    let crud_rows = env_u64("MM_CRUD_ROWS", 100_000);
    println!("## Table 3 — CRUD stress: unidb (SQL) vs Postgres (relational)\n");
    println!(
        "Full CRUD at matched durability — not just INSERT: bulk insert, filtered and\n\
         grouped SELECT, bulk UPDATE, selected and full DELETE. Each row shows how many\n\
         records the operation touched and its throughput (records/sec). Table pre-loaded\n\
         to **{crud_rows} rows** (`MM_CRUD_ROWS` to change).\n\
         \n\
         **Note on INSERT:** here each row is its **own durable commit** (one fsync/row —\n\
         the per-row latency floor, ~hundreds/sec), which is why it is far below the\n\
         batched bulk-load path in Table 3.1 (one commit per {BULK_COMMIT_BATCH} rows).\n"
    );
    if let Some(ref m) = pg_method {
        println!(
            "_Durability lens: unidb `{sync_prim}`, Postgres `wal_sync_method={m}` (matched)._\n"
        );
        let sdir = tempdir().unwrap();
        let se = Arc::new(bench_engine_open(sdir.path()));
        se.set_deferred_sync(true);
        let u = url.as_deref().unwrap();
        phased("t3_build", || {
            sql_build_crud(&se, crud_rows);
            let _ = pg_build_crud(u, crud_rows);
        });

        let n = crud_rows;
        let half = (n / 2) as i64;
        let base = n as i64;
        println!(
            "Extra columns (C1, unidb only): **WAL B/row** = cumulative WAL bytes the op\n\
             appended ÷ records touched (the index-maintenance proof — a `body`-only UPDATE\n\
             should append ~0 index bytes once unchanged indexed columns are skipped);\n\
             **dec/row** = full-row heap decodes ÷ records touched (exposes full-scan waste\n\
             on the write path — a selective op that decodes every row shows dec/row ≫ 1);\n\
             **cols/row** = column *values* materialized into a Literal ÷ records touched\n\
             (Phase B decode-pushdown proof — falls as unreferenced columns, esp. TEXT,\n\
             stop being materialized).\n"
        );
        println!("| operation | records | unidb (rec/s) | postgres (rec/s) | unidb ÷ PG | remark (winner · margin) | WAL B/row | dec/row | cols/row |");
        println!("|-----------|--------:|--------------:|-----------------:|-----------:|:-------------------------|----------:|--------:|---------:|");
        crud_row_c1(
            "INSERT (per-row commit)",
            phased("t3_insert_unidb", || {
                measured_unidb(&se, || sql_crud_insert(&se, n, base))
            }),
            phased("t3_insert_pg", || pg_crud_insert(u, n, base)),
        );
        // Refresh statistics on both engines now the table is at full size, so
        // each planner makes a stats-informed choice for the UPDATE/DELETE below
        // — unidb's A3 index-vs-scan gate (a selective range takes the B-tree, a
        // non-selective one the sequential scan) and Postgres's planner alike.
        // This is the realistic production state and fairer than comparing an
        // analyzed engine against an un-analyzed one. Untimed setup, like the
        // pre-grow.
        phased("t3_analyze", || {
            let ax = se.begin().unwrap();
            let _ = se.execute_sql(ax, "ANALYZE t");
            se.commit(ax).unwrap();
            if let Ok(mut c) = pg_dial(u) {
                let _ = c.batch_execute("ANALYZE t");
            }
        });
        // Selectivity fixed at 5% (k < N/20) — matches realistic filtered-SELECT usage.
        // The previous k < N (100% match) was the worst case for filtering optimisations
        // and hid the real gap: at 100% selectivity every row materialises regardless.
        // Verified 2026-07-17 by Fable architectural analysis (cost breakdown in PROGRESS.md).
        let sel_hi = (n / 20) as i64; // 5 % of rows
        crud_row_c1(
            "SELECT filtered (k<N/20, 5%)",
            phased("t3_selfilt_unidb", || {
                measured_unidb(&se, || sql_crud_select_filtered(&se, 0, sel_hi))
            }),
            phased("t3_selfilt_pg", || pg_crud_select_filtered(u, 0, sel_hi)),
        );
        crud_row_c1(
            "SELECT grouped (GROUP BY g)",
            phased("t3_selgrp_unidb", || {
                measured_unidb(&se, || sql_crud_select_grouped(&se, 2 * n))
            }),
            phased("t3_selgrp_pg", || pg_crud_select_grouped(u, 2 * n)),
        );
        crud_row_c1(
            "SELECT COUNT(*) (all)",
            phased("t3_countall_unidb", || {
                measured_unidb(&se, || sql_crud_count_all(&se, 2 * n))
            }),
            phased("t3_countall_pg", || pg_crud_count_all(u, 2 * n)),
        );
        crud_row_c1(
            "UPDATE bulk (k<N/2)",
            phased("t3_update_unidb", || {
                measured_unidb(&se, || sql_crud_update_bulk(&se, half))
            }),
            phased("t3_update_pg", || pg_crud_update_bulk(u, half)),
        );
        crud_row_c1(
            "DELETE selected (k>=N)",
            phased("t3_delsel_unidb", || {
                measured_unidb(&se, || sql_crud_delete_selected(&se, n as i64))
            }),
            phased("t3_delsel_pg", || pg_crud_delete_selected(u, n as i64)),
        );
        crud_row_c1(
            "DELETE all",
            phased("t3_delall_unidb", || {
                measured_unidb(&se, || sql_crud_delete_all(&se))
            }),
            phased("t3_delall_pg", || pg_crud_delete_all(u)),
        );
        println!();
        println!(
            "### Table 3 — Known honest ceilings (verified; do not re-investigate without new evidence)\n\
             \n\
             | operation | current ratio | ceiling | root cause | revisit when |\n\
             |---|---|---|---|---|\n\
             | SELECT filtered | ~0.57× (was 100% selectivity bench — misleading) | TBD at 5% | **Bench fixed 2026-07-17**: previous query matched 100% of rows (k<N), the worst case for any filter optimisation. Re-benched at k<N/20 (5% selectivity). Real gap drivers: 4 heap allocs/row (42%), interpreted predicate column-name scan (27%), COLS_DECODED atomics (10%). SIMD NO-GO (scatter layout, nightly-only API). JIT NO-GO (Cranelift, unsafe, poor ROI). Addressable via: column pre-binding + remove COLS_DECODED + late materialisation. | Ship item 59 (column pre-binding + late materialisation) then re-measure. |\n\
             | UPDATE bulk | ~0.04–0.07× | ~0.07–0.09× (with HOT) | B-tree per-row insert (~10–15 µs/row) dominates regardless of WAL batching — proved by item 56 Step 2 (2026-07-17): WAL savings −30% but throughput regressed due to staging pressure. Only HOT (D4 sign-off) changes this. | D4 sign-off granted in PROGRESS.md. |\n\
             | INSERT per-row | ~0.54× | ~0.55–0.60× | Per-row fsync floor + PG scale advantages. Step 4 (logical B-tree WAL, 8837→655 B/row) delivered the addressable gain (2026-07-17). | Batch-commit or group-commit mode. |\n\
             | DELETE selected | ~0.07× | ~0.07× | After Step 3 (WAL_XMAX_BATCH), bottleneck is `delete_many` page-write phase, not the scan. Parallel scan tried (item 57, 2026-07-17) — zero improvement at 50% selectivity. Only further WAL compression or HOT changes this. | New WAL record type reducing page-write overhead. |\n"
        );
    } else {
        println!(
            "_`PG_URL` unset → Postgres columns skipped. Set it (superuser conn) to fill this table._\n"
        );
    }

    // ---- Table 3.1: bulk stress — insert + full-scan select at scale ----
    let bulk_sizes: Vec<u64> = std::env::var("MM_BULK_SIZES")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![10_000, 1_000_000, 2_000_000]);
    println!("## Table 3.1 — Bulk stress: insert + full-scan select at scale\n");
    println!(
        "Scaling behaviour of a single-table load and a full-table scan as the row count\n\
         climbs. For each size a **fresh** table is built, `n` rows are bulk-inserted\n\
         (batched prepared single-row inserts, one durable commit per {BULK_COMMIT_BATCH}\n\
         rows — the same method on both engines), then a full-**heap** scan is timed\n\
         (`COUNT(*) WHERE body <> 'x'` — a predicate on the non-indexed `body` column, so\n\
         neither engine can serve it index-only; this measures real scan throughput, not a\n\
         count optimizer). Throughput is records/sec. Sizes swept: `{bulk_sizes:?}` (`MM_BULK_SIZES`\n\
         to override — e.g. push to `5000000` or `10000000` for a heavier run). The default\n\
         tops out at 2M to keep a full report reasonable; the engine handles ≥10M.\n\
         \n\
         **On the scan gap at scale:** Postgres runs a **parallel** sequential scan once the\n\
         table crosses its parallel threshold (`max_parallel_workers_per_gather` is capped at 2\n\
         in Table 3 SELECT ops for cross-environment comparability; Table 3.1 uses the server\n\
         default, so a large scan-side lead here reflects PG's parallel degree, not per-row\n\
         storage speed).\n"
    );
    if let Some(ref m) = pg_method {
        println!(
            "_Durability lens: unidb `{sync_prim}`, Postgres `wal_sync_method={m}` (matched)._\n"
        );
        println!(
            "| rows | unidb insert (rec/s) | postgres insert (rec/s) | insert winner · margin | unidb scan (rec/s) | postgres scan (rec/s) | scan winner · margin |"
        );
        println!(
            "|-----:|---------------------:|------------------------:|:-----------------------|-------------------:|----------------------:|:---------------------|"
        );
        let u = url.as_deref().unwrap();
        for &n in &bulk_sizes {
            eprintln!("[mmreport] Table 3.1 bulk at {n} rows…");
            let bdir = tempdir().unwrap();
            let be = Arc::new(bench_engine_open(bdir.path()));
            be.set_deferred_sync(true);
            let ui = phased(&format!("t31_insert_unidb_{n}"), || sql_bulk_insert(&be, n));
            let pi = phased(&format!("t31_insert_pg_{n}"), || pg_bulk_insert(u, n));
            let us = phased(&format!("t31_select_unidb_{n}"), || sql_bulk_select(&be, n));
            let ps = phased(&format!("t31_select_pg_{n}"), || pg_bulk_select(u, n));
            let (ui_r, pi_r) = (rps(ui.0, ui.1), rps(pi.0, pi.1));
            let (us_r, ps_r) = (rps(us.0, us.1), rps(ps.0, ps.1));
            println!(
                "| {n} | {ui_r:.0} | {pi_r:.0} | {} | {us_r:.0} | {ps_r:.0} | {} |",
                winner_remark(ui_r, pi_r),
                winner_remark(us_r, ps_r),
            );
        }
        println!();
    } else {
        println!("_`PG_URL` unset → unidb-only bulk numbers (no Postgres comparison)._\n");
        println!("| rows | unidb insert (rec/s) | unidb scan (rec/s) |");
        println!("|-----:|---------------------:|-------------------:|");
        for &n in &bulk_sizes {
            eprintln!("[mmreport] Table 3.1 bulk at {n} rows…");
            let bdir = tempdir().unwrap();
            let be = Arc::new(bench_engine_open(bdir.path()));
            be.set_deferred_sync(true);
            let ui = phased(&format!("t31_insert_unidb_{n}"), || sql_bulk_insert(&be, n));
            let us = phased(&format!("t31_select_unidb_{n}"), || sql_bulk_select(&be, n));
            println!("| {n} | {:.0} | {:.0} |", rps(ui.0, ui.1), rps(us.0, us.1));
        }
        println!();
    }

    // ---- Table 4: unidb multi-model (1 txn) vs the replaced stack, tx-count sweep ----
    // `MM_REPLACED_STACK=1` adds the honest §6 headline column: the SAME four
    // model-writes run across four independent PG systems (row + pgvector + graph
    // + queue) with no shared transaction. Without it, only the PG-relational
    // reference floor is shown (back-compat).
    let replaced_stack = std::env::var("MM_REPLACED_STACK").ok().as_deref() == Some("1");
    println!("## Table 4 — unidb multi-model (1 atomic txn) vs the replaced stack, at scale\n");
    if replaced_stack {
        println!(
            "The §6 headline. unidb commits **four model-writes in ONE atomic transaction**\n\
             (relational + `VECTOR(128)`+HNSW + graph edge + event) — **1 `fsync`, all-or-\n\
             nothing**. The **replaced stack** does the *same four writes* as **four\n\
             independent systems with no shared transaction** (Postgres row + pgvector\n\
             +HNSW + a graph adjacency table + an outbox queue), each its own connection and\n\
             its own durable commit — **4 `fsync`s, 4 round-trips, and NO cross-system\n\
             atomicity** (a crash mid-sequence leaves a torn record; see the\n\
             crash-consistency test). The `PG relational only` column is the stack's\n\
             single-model *floor* (one write), kept for reference — it is **not** the\n\
             baseline. This is a **conservative** proxy: real Neo4j/Kafka/Qdrant are heavier\n\
             than PG tables, so the true stack tax is larger.\n"
        );
    } else {
        println!(
            "unidb commits **four model-writes in one atomic transaction** (relational +\n\
             `VECTOR(128)` + graph edge + event); the `PG relational only` column does the\n\
             **relational INSERT only** (1 model-write) — the stack's single-model floor, not\n\
             the real baseline. Set **`MM_REPLACED_STACK=1`** (needs a pgvector-enabled\n\
             Postgres) to add the honest §6 replaced-stack column: the same four writes across\n\
             four independent systems with no shared transaction.\n"
        );
    }
    let sweep: Vec<u64> = std::env::var("MM_TX_SWEEP")
        .ok()
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![1_000, 10_000, 100_000, 1_000_000]);
    if let Some(ref m) = pg_method {
        println!(
            "_Durability lens: unidb `{sync_prim}`, Postgres `wal_sync_method={m}` (matched);\n\
             single writer, so no group-commit coalescing on either side._\n"
        );
        let u = url.as_deref().unwrap();
        if replaced_stack {
            println!(
                "| txns | unidb txns/s | unidb ms/txn | stack (4-sys) txns/s | stack ms/txn | **unidb ÷ stack** | PG relational only txns/s |"
            );
            println!(
                "|-----:|-------------:|-------------:|---------------------:|-------------:|:-----------------:|--------------------------:|"
            );
            for &c in &sweep {
                eprintln!("[mmreport] Table 4 at {c} txns…");
                let uw4 = phased(&format!("t4_unidb_{c}"), || unidb_w4_throughput(c));
                let stack = phased(&format!("t4_stack_{c}"), || {
                    pg_replaced_stack_throughput(u, c)
                });
                let pgr = phased(&format!("t4_pg_{c}"), || pg_relational_throughput(u, c));
                let u_ms = if uw4 > 0.0 { 1000.0 / uw4 } else { 0.0 };
                match stack {
                    Some(s) if s > 0.0 => {
                        let ratio = uw4 / s;
                        let s_ms = 1000.0 / s;
                        println!(
                            "| {c} | {uw4:.0} | {u_ms:.3} | {s:.0} | {s_ms:.3} | **{ratio:.2}×** | {pgr:.0} |"
                        );
                    }
                    _ => {
                        println!(
                            "| {c} | {uw4:.0} | {u_ms:.3} | _(pgvector n/a)_ | — | — | {pgr:.0} |"
                        );
                    }
                }
            }
            println!();
            println!(
                "`unidb ÷ stack > 1` means unidb's single atomic commit beats the four-system\n\
                 dual-write on throughput. **The throughput edge is `fsync`-cost-dependent:**\n\
                 unidb pays 1 durable sync per record, the stack 4 — but that advantage only\n\
                 dominates when a durable sync is *expensive*. Under Docker's shared-VM `fsync`\n\
                 (cheap, buffered — see the durability caveat) the per-model HNSW/index CPU\n\
                 paid on **both** sides dominates instead, so the ratio sits near parity and\n\
                 the win narrows; on a native host with a real flush-to-platter sync the\n\
                 4→1 collapse is worth far more. The **unconditional** win is the crash-\n\
                 consistency below — one atomic commit vs four with no shared transaction —\n\
                 which no `fsync` setting changes.\n"
            );
            // Crash-consistency face: the qualitative half no fsync tuning fixes.
            match pg_stack_torn_record_demo(u) {
                Some(true) => println!(
                    "**Crash-consistency:** the replaced stack recovered a **torn record** — the\n\
                     relational row is durably present while its embedding/edge/event are absent\n\
                     (no transaction spans the four systems to undo it). unidb recovers **0\n\
                     orphans** by construction — a crash before `WAL_TXN_COMMIT` undoes all four,\n\
                     proven in `tests/crash` (`item16_incomplete_four_model_txn_leaves_zero_orphans`,\n\
                     `item16_committed_four_model_txn_survives_intact`). No fsync setting buys the\n\
                     stack this.\n"
                ),
                Some(false) => println!(
                    "_Crash-consistency demo inconclusive on this run (state unexpectedly clean)._\n"
                ),
                None => {}
            }
        } else {
            println!(
                "| txns | model-writes/txn | unidb txns/s | unidb ms/txn | PG relational only txns/s | postgres ms/txn | unidb ÷ PG-floor |"
            );
            println!(
                "|-----:|:----------------:|-------------:|-------------:|--------------------------:|----------------:|-----------------:|"
            );
            for &c in &sweep {
                eprintln!("[mmreport] Table 4 at {c} txns…");
                let uw4 = phased(&format!("t4_unidb_{c}"), || unidb_w4_throughput(c));
                let pgr = phased(&format!("t4_pg_{c}"), || pg_relational_throughput(u, c));
                let ratio = if pgr > 0.0 { uw4 / pgr } else { 0.0 };
                let u_ms = if uw4 > 0.0 { 1000.0 / uw4 } else { 0.0 };
                let p_ms = if pgr > 0.0 { 1000.0 / pgr } else { 0.0 };
                println!(
                    "| {c} | 4 : 1 | {uw4:.0} | {u_ms:.3} | {pgr:.0} | {p_ms:.3} | {ratio:.2}× |"
                );
            }
            println!();
        }
    } else {
        println!("_`PG_URL` unset → Postgres columns skipped; set it to run Table 4._\n");
    }

    // Restore Postgres's default wal_sync_method (shared lens for Tables 3 & 4).
    if pg_method.is_some() {
        if let Some(u) = url.as_deref() {
            pg_reset_lens(u);
        }
    }

    // ---- Table 5: PK/FK relational-integrity stress ----
    let fk_orders = env_u64("MM_FK_ORDERS", 20_000);
    println!("## Table 5 — PK/FK relational-integrity stress: unidb vs Postgres\n");
    println!(
        "A realistic two-table schema — `customers (id PRIMARY KEY, name)` and\n\
         `orders (id PRIMARY KEY, customer_id REFERENCES customers(id), amount,\n\
         status)` — pre-loaded with **{FK_CUSTOMERS} customers**, then\n\
         **{fk_orders} orders** (`MM_FK_ORDERS` to change) each referencing a real\n\
         customer. unidb enforces row-level FK existence via the parent's implicit\n\
         unique-index B-tree (O(log n) per check, item 36, 2026-07-14) and RESTRICT\n\
         on a still-referenced parent DELETE — the same shape of check Postgres has\n\
         always done. Before item 36 this table would have been an unfair\n\
         comparison (unidb only checked the *table* existed, not the *row*); now\n\
         both sides pay a real, comparable integrity-check cost.\n"
    );
    let fk_pg_method = url.as_deref().and_then(pg_ensure_lens);
    if let Some(ref m) = fk_pg_method {
        println!(
            "_Durability lens: unidb `{sync_prim}`, Postgres `wal_sync_method={m}` (matched)._\n"
        );
        let u = url.as_deref().unwrap();
        let fdir = tempdir().unwrap();
        let fe = Arc::new(bench_engine_open(fdir.path()));
        fe.set_deferred_sync(true);
        phased("t5_build", || {
            sql_fk_setup(&fe, FK_CUSTOMERS);
            let _ = pg_fk_setup(u, FK_CUSTOMERS);
        });

        println!(
            "| operation | records | unidb (rec/s) | postgres (rec/s) | unidb ÷ PG | remark (winner · margin) |"
        );
        println!(
            "|-----------|--------:|--------------:|-----------------:|-----------:|:-------------------------|"
        );
        let (uc, us) = phased("t5_insert_unidb", || {
            sql_fk_insert_valid(&fe, fk_orders, 0, FK_CUSTOMERS)
        });
        let (pc, ps) = phased("t5_insert_pg", || {
            pg_fk_insert_valid(u, fk_orders, 0, FK_CUSTOMERS)
        });
        let (uu, pp) = (rps(uc, us), rps(pc, ps));
        println!(
            "| INSERT valid FK (per-row commit) | {} | {uu:.0} | {pp:.0} | {:.2}× | {} |",
            uc.max(pc),
            if pp > 0.0 { uu / pp } else { 0.0 },
            winner_remark(uu, pp),
        );

        let half = (fk_orders / 2) as i64;
        let (uc, us) = phased("t5_update_unidb", || sql_fk_update_bulk(&fe, half));
        let (pc, ps) = phased("t5_update_pg", || pg_fk_update_bulk(u, half));
        let (uu, pp) = (rps(uc, us), rps(pc, ps));
        println!(
            "| UPDATE bulk (re-checks FK path) | {} | {uu:.0} | {pp:.0} | {:.2}× | {} |",
            uc.max(pc),
            if pp > 0.0 { uu / pp } else { 0.0 },
            winner_remark(uu, pp),
        );

        let (uc, us) = phased("t5_join_unidb", || sql_fk_join_select(&fe));
        let (pc, ps) = phased("t5_join_pg", || pg_fk_join_select(u));
        let (uu, pp) = (rps(uc, us), rps(pc, ps));
        println!(
            "| SELECT JOIN orders/customers | {} | {uu:.0} | {pp:.0} | {:.2}× | {} |",
            uc.max(pc),
            if pp > 0.0 { uu / pp } else { 0.0 },
            winner_remark(uu, pp),
        );
        println!();

        println!("**Correctness (not a speed number — a pass/fail proof both engines enforce integrity):**\n");
        let sql_rejects = phased("t5_reject_unidb", || sql_fk_rejects_invalid(&fe));
        let pg_rejects = phased("t5_reject_pg", || pg_fk_rejects_invalid(u));
        println!(
            "- INSERT referencing a non-existent customer: unidb {}, Postgres {}",
            if sql_rejects {
                "**rejected** ✓"
            } else {
                "accepted ✗"
            },
            if pg_rejects {
                "**rejected** ✓"
            } else {
                "accepted ✗"
            },
        );
        let sql_restrict = phased("t5_restrict_unidb", || {
            sql_fk_restrict_blocks_delete(&fe, 0)
        });
        let pg_restrict = phased("t5_restrict_pg", || pg_fk_restrict_blocks_delete(u, 0));
        println!(
            "- DELETE of a still-referenced customer: unidb {}, Postgres {}\n",
            if sql_restrict {
                "**blocked (RESTRICT)** ✓"
            } else {
                "allowed ✗"
            },
            if pg_restrict {
                "**blocked (RESTRICT)** ✓"
            } else {
                "allowed ✗"
            },
        );
    } else {
        println!("_`PG_URL` unset → Postgres columns skipped; set it to run Table 5._\n");
        let fdir = tempdir().unwrap();
        let fe = Arc::new(bench_engine_open(fdir.path()));
        fe.set_deferred_sync(true);
        phased("t5_build_unidb_only", || sql_fk_setup(&fe, FK_CUSTOMERS));
        let (uc, us) = phased("t5_insert_unidb_only", || {
            sql_fk_insert_valid(&fe, fk_orders, 0, FK_CUSTOMERS)
        });
        println!("| operation | records | unidb (rec/s) |");
        println!("|-----------|--------:|---------------:|");
        println!(
            "| INSERT valid FK (per-row commit) | {uc} | {:.0} |",
            rps(uc, us)
        );
        let sql_rejects = phased("t5_reject_unidb_only", || sql_fk_rejects_invalid(&fe));
        println!(
            "\n- INSERT referencing a non-existent customer: unidb {}\n",
            if sql_rejects {
                "**rejected** ✓"
            } else {
                "accepted ✗"
            },
        );
    }
    if fk_pg_method.is_some() {
        if let Some(u) = url.as_deref() {
            pg_reset_lens(u);
        }
    }

    // ---- Caveats ----
    println!("## Caveats\n");
    println!(
        "- Single node, `{sync_prim}` commit sync, group-commit on. The per-commit floor\n\
         is this storage's fsync latency.\n\
         - Sizes swept: `{sizes:?}` (`MM_SIZES` to override — e.g. millions). W2–W4 build\n\
         the ANN/graph indexes synchronously, so large pre-grows are slow **by design**\n\
         — that cost is exactly what the async-derivation design (parked) would move off\n\
         the commit path if `W4/W0` is seen rising.\n\
         - Marginal-commit sample = {sample} (`MM_SAMPLE`); numbers carry a few-percent\n\
         noise, the *trend across sizes* is the signal.\n\
         - Table 5's order count = {fk_orders} (`MM_FK_ORDERS` to override); its FK\n\
         check is single-column, point-lookup (item 35's implicit unique index) —\n\
         a composite or non-indexable FK column falls back to an O(n) heap scan on\n\
         unidb (documented limitation, not exercised by this table).\n"
    );
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
        "mmreport" => {
            bench_mm_report();
            return;
        }
        "b2" => {
            // Run only the B2 CRUD criterion group (skips the slow ladder).
            let mut criterion = Criterion::default().configure_from_args();
            bench_pg_crud(&mut criterion);
            criterion.final_summary();
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
