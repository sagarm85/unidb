// Concurrency-correctness matrix (border cases) — the pass/fail stress table
// behind the item-16 investigation (`docs/backlog/backlog_index.md` "Next up"
// item 16: MVCC visibility anomaly under `UNIDB_CONCURRENT_SQL_WRITES` — the
// `cross_row_update_deadlock_resolves_no_hang` shape can surface 3 visible rows
// instead of 2 under CPU contention).
//
// Unlike the throughput benches, every cell here is a CORRECTNESS check: it
// runs a production-shaped concurrent read/write workload and asserts an
// invariant oracle afterwards (and, for reader cells, *during* the run). The
// matrix sweeps the permutations that matter:
//
//   toggle    UNIDB_CONCURRENT_SQL_WRITES on (production default) / off (fallback)
//   index     B-tree-indexed table / unindexed (isolates the index write path)
//   isolation reader snapshots at READ COMMITTED / REPEATABLE READ / SERIALIZABLE
//   workload  insert storm · cross-row UPDATE churn (the item-16 shape) ·
//             same-row contention · mixed INSERT/UPDATE/DELETE · readers-during-
//             churn · parallel-scan readers · balance-transfer sum invariant ·
//             vacuum interleaved with churn · delete+reinsert (slot reuse)
//
// Degenerate combinations are deliberately not enumerated (e.g. the isolation
// sweep only runs where a reader snapshot is what's under test); every cell
// that IS listed is a case a production application actually hits.
//
// The item-16 anomaly is intermittent and needs CPU contention to fire, so the
// harness (a) runs background spinner threads to oversubscribe the cores and
// (b) repeats every cell; a cell FAILs if ANY repeat violates its oracle.
//
// Output: a self-contained markdown section (table + legend + caveats) on
// stdout. `scripts/report.sh` appends it to the benchmark report.
//
// Knobs (all env, all optional):
//   CONC_REPEATS  repeats per cell            (default 3)
//   CONC_SPIN     CPU-contention spinner threads (default = cores; 0 disables)
//   CONC_ROUNDS   workload-size multiplier, integer (default 1)
//   CONC_ONLY     substring filter on scenario id (e.g. "churn")
//   CONC_DEADLINE per-repeat hang deadline, seconds (default 120)
//   CONC_STRICT   "1" → exit 1 if any cell fails (default: always exit 0 so a
//                 known intermittent failure doesn't abort report generation)
//
// Run directly:  cargo bench --bench conc_matrix
// Via report:    scripts/report.sh --conc

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::Duration;

use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::{DbError, Engine, Isolation, SqlResult};

// ── knobs ────────────────────────────────────────────────────────────────────

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn repeats() -> usize {
    env_usize("CONC_REPEATS", 3).max(1)
}

fn rounds_mult() -> usize {
    env_usize("CONC_ROUNDS", 1).max(1)
}

fn deadline_secs() -> u64 {
    env_usize("CONC_DEADLINE", 120).max(10) as u64
}

// ── engine + SQL helpers ─────────────────────────────────────────────────────

/// Fresh engine in a tempdir, group-commit mode (like the server), with the
/// concurrent-SQL-writes toggle set per the cell. The tempdir is returned so
/// it lives as long as the engine.
fn open_engine(toggle_on: bool) -> (tempfile::TempDir, Arc<Engine>) {
    let dir = tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    engine.set_deferred_sync(true);
    engine.set_concurrent_sql_writes(toggle_on);
    (dir, engine)
}

fn ddl(engine: &Engine, stmts: &[&str]) {
    let x = engine.begin().unwrap();
    for s in stmts {
        engine.execute_sql(x, s).unwrap();
    }
    engine.commit(x).unwrap();
}

/// Errors a correct application retries; anything else is a genuine failure.
fn retryable(e: &DbError) -> bool {
    matches!(
        e,
        DbError::Deadlock { .. }
            | DbError::WriteConflict { .. }
            | DbError::SerializationFailure { .. }
    )
}

/// Run one or more statements in one transaction with retry-on-conflict.
/// Returns Err(msg) only on a NON-retryable error.
fn txn_retry(engine: &Engine, iso: Isolation, stmts: &[String]) -> Result<(), String> {
    loop {
        let xid = engine
            .begin_with_isolation(iso)
            .map_err(|e| format!("begin: {e:?}"))?;
        let mut conflicted = false;
        for s in stmts {
            match engine.execute_sql(xid, s) {
                Ok(_) => {}
                Err(e) if retryable(&e) => {
                    let _ = engine.abort(xid);
                    conflicted = true;
                    break;
                }
                Err(e) => {
                    let _ = engine.abort(xid);
                    return Err(format!("{s}: {e:?}"));
                }
            }
        }
        if conflicted {
            continue;
        }
        match engine.commit(xid) {
            Ok(_) => return Ok(()),
            Err(e) if retryable(&e) => continue,
            Err(e) => return Err(format!("commit: {e:?}")),
        }
    }
}

/// First projected column of every row as i64, in one read-committed txn.
fn select_i64s(engine: &Engine, sql: &str) -> Result<Vec<i64>, String> {
    select_i64s_in(engine, None, sql)
}

/// Like `select_i64s` but inside an existing txn when `xid` is given.
fn select_i64s_in(engine: &Engine, xid: Option<u64>, sql: &str) -> Result<Vec<i64>, String> {
    let (x, own) = match xid {
        Some(x) => (x, false),
        None => (engine.begin().map_err(|e| format!("begin: {e:?}"))?, true),
    };
    let res = engine.execute_sql(x, sql).map_err(|e| {
        if own {
            let _ = engine.abort(x);
        }
        format!("{sql}: {e:?}")
    })?;
    if own {
        engine.commit(x).map_err(|e| format!("commit: {e:?}"))?;
    }
    match res.into_iter().next() {
        Some(SqlResult::Rows { rows, .. }) => rows
            .into_iter()
            .map(|r| match r.first() {
                Some(Literal::Int(n)) => Ok(*n),
                o => Err(format!("{sql}: expected Int, got {o:?}")),
            })
            .collect(),
        o => Err(format!("{sql}: expected Rows, got {o:?}")),
    }
}

fn count_star(engine: &Engine, xid: Option<u64>, table: &str) -> Result<i64, String> {
    let v = select_i64s_in(engine, xid, &format!("SELECT COUNT(*) FROM {table}"))?;
    v.first()
        .copied()
        .ok_or_else(|| "COUNT(*) returned no row".into())
}

/// Assert the id set visible in one snapshot is exactly `expected`.
fn assert_id_set(got: &mut Vec<i64>, expected: &BTreeSet<i64>, what: &str) -> Result<(), String> {
    got.sort_unstable();
    let before = got.len();
    got.dedup();
    if got.len() != before {
        return Err(format!("{what}: duplicate ids in one snapshot"));
    }
    let got_set: BTreeSet<i64> = got.iter().copied().collect();
    if &got_set != expected {
        let extra: Vec<_> = got_set.difference(expected).collect();
        let missing: Vec<_> = expected.difference(&got_set).collect();
        return Err(format!(
            "{what}: visible {} rows, expected {} (extra {extra:?}, missing {missing:?})",
            got_set.len(),
            expected.len()
        ));
    }
    Ok(())
}

/// Index-vs-scan agreement on an indexed `k` column: for every distinct k the
/// scan reports, the index-served `WHERE k = <v>` lookup must return exactly
/// the same id set — a lost/duplicated/stale index entry fails loudly here.
fn assert_index_agrees_with_scan(engine: &Engine, table: &str) -> Result<(), String> {
    let x = engine.begin().map_err(|e| format!("begin: {e:?}"))?;
    let ids = select_i64s_in(engine, Some(x), &format!("SELECT id FROM {table}"))?;
    let ks = select_i64s_in(engine, Some(x), &format!("SELECT k FROM {table}"))?;
    engine.commit(x).map_err(|e| format!("commit: {e:?}"))?;
    if ids.len() != ks.len() {
        return Err("scan projected id/k row counts differ".into());
    }
    let mut by_k: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    for (id, k) in ids.iter().zip(ks.iter()) {
        by_k.entry(*k).or_default().insert(*id);
    }
    for (k, want) in &by_k {
        let got: BTreeSet<i64> =
            select_i64s(engine, &format!("SELECT id FROM {table} WHERE k = {k}"))?
                .into_iter()
                .collect();
        if &got != want {
            return Err(format!(
                "index lookup k={k}: got {got:?}, scan says {want:?}"
            ));
        }
    }
    Ok(())
}

// ── workloads ────────────────────────────────────────────────────────────────
// Each returns Ok(()) or Err(first invariant violation). Writer threads report
// errors through a shared slot instead of panicking so a violation is a table
// row, not an aborted run.

type Shared = Arc<std::sync::Mutex<Option<String>>>;

fn note_err(slot: &Shared, msg: String) {
    let mut g = slot.lock().unwrap();
    if g.is_none() {
        *g = Some(msg);
    }
}

fn take_err(slot: &Shared) -> Result<(), String> {
    match slot.lock().unwrap().take() {
        Some(m) => Err(m),
        None => Ok(()),
    }
}

/// W1 — insert storm: `writers` threads insert disjoint ids with overlapping
/// keys. Oracle: every committed row visible exactly once; index agrees.
fn w_insert_storm(toggle: bool, indexed: bool) -> Result<(), String> {
    let (_d, engine) = open_engine(toggle);
    let mut stmts = vec!["CREATE TABLE t (id INT, k INT, body TEXT)"];
    if indexed {
        stmts.push("CREATE INDEX t_k ON t USING BTREE (k)");
    }
    ddl(&engine, &stmts);

    let writers = 8usize;
    let per = 120i64 * rounds_mult() as i64;
    let n_keys = 23i64;
    let barrier = Arc::new(Barrier::new(writers));
    let err: Shared = Arc::default();
    let mut handles = Vec::new();
    for t in 0..writers {
        let engine = Arc::clone(&engine);
        let barrier = Arc::clone(&barrier);
        let err = Arc::clone(&err);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..per {
                let id = (t as i64) * 100_000 + i;
                let s = format!(
                    "INSERT INTO t (id, k, body) VALUES ({id}, {}, 'b{id}')",
                    id % n_keys
                );
                if let Err(m) = txn_retry(&engine, Isolation::ReadCommitted, &[s]) {
                    note_err(&err, m);
                    return;
                }
            }
        }));
    }
    for h in handles {
        h.join().map_err(|_| "writer panicked".to_string())?;
    }
    take_err(&err)?;

    let expected: BTreeSet<i64> = (0..writers as i64)
        .flat_map(|t| (0..per).map(move |i| t * 100_000 + i))
        .collect();
    let mut got = select_i64s(&engine, "SELECT id FROM t")?;
    assert_id_set(&mut got, &expected, "post-storm scan")?;
    let c = count_star(&engine, None, "t")?;
    if c != expected.len() as i64 {
        return Err(format!("COUNT(*)={c}, expected {}", expected.len()));
    }
    if indexed {
        assert_index_agrees_with_scan(&engine, "t")?;
    }
    Ok(())
}

/// Seed `rows` ids (1..=rows) with k = 10*id; used by the churn workloads.
/// Inserts are chunked so a large seed never produces one enormous statement.
fn seed_rows(engine: &Engine, indexed: bool, rows: i64) {
    let mut stmts = vec!["CREATE TABLE t (id INT, k INT)".to_string()];
    if indexed {
        stmts.push("CREATE INDEX t_k ON t USING BTREE (k)".to_string());
    }
    let mut i = 1i64;
    while i <= rows {
        let hi = (i + 499).min(rows);
        let values: Vec<String> = (i..=hi).map(|j| format!("({j}, {})", 10 * j)).collect();
        stmts.push(format!(
            "INSERT INTO t (id, k) VALUES {}",
            values.join(", ")
        ));
        i = hi + 1;
    }
    let refs: Vec<&str> = stmts.iter().map(|s| s.as_str()).collect();
    ddl(engine, &refs);
}

/// Cross-row UPDATE churn: each writer repeatedly updates a PAIR of rows in one
/// txn, threads taking opposite orders so the lock manager can form cycles.
/// The logical row count never changes — this is the item-16 shape.
fn churn_writers(
    engine: &Arc<Engine>,
    writers: usize,
    rows: i64,
    rounds: i64,
    err: &Shared,
) -> Vec<thread::JoinHandle<()>> {
    let barrier = Arc::new(Barrier::new(writers));
    let mut handles = Vec::new();
    for w in 0..writers {
        let engine = Arc::clone(engine);
        let barrier = Arc::clone(&barrier);
        let err = Arc::clone(err);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for r in 0..rounds {
                let a = (r + w as i64) % rows + 1;
                let b = (r + w as i64 + 1) % rows + 1;
                let (first, second) = if w % 2 == 0 { (a, b) } else { (b, a) };
                let v = 100 + r;
                let stmts = [
                    format!("UPDATE t SET k = {v} WHERE id = {first}"),
                    format!("UPDATE t SET k = {v} WHERE id = {second}"),
                ];
                if let Err(m) = txn_retry(&engine, Isolation::ReadCommitted, &stmts) {
                    note_err(&err, m);
                    return;
                }
            }
        }));
    }
    handles
}

/// W2 — cross-row churn, post-hoc oracle: exactly the seeded rows remain
/// visible (the item-16 anomaly = an extra superseded version in the scan),
/// COUNT(*) agrees (the B1 header-only fast path), and the index agrees.
fn w_cross_row_churn(toggle: bool, indexed: bool, writers: usize, rows: i64) -> Result<(), String> {
    let (_d, engine) = open_engine(toggle);
    seed_rows(&engine, indexed, rows);
    let rounds = 60 * rounds_mult() as i64;
    let err: Shared = Arc::default();
    for h in churn_writers(&engine, writers, rows, rounds, &err) {
        h.join().map_err(|_| "writer panicked".to_string())?;
    }
    take_err(&err)?;

    let expected: BTreeSet<i64> = (1..=rows).collect();
    let mut got = select_i64s(&engine, "SELECT id FROM t")?;
    assert_id_set(&mut got, &expected, "post-churn scan")?;
    let c = count_star(&engine, None, "t")?;
    if c != rows {
        return Err(format!("COUNT(*)={c}, expected {rows}"));
    }
    if indexed {
        assert_index_agrees_with_scan(&engine, "t")?;
    }
    Ok(())
}

/// W3 — same-row contention: every writer hammers the SAME logical row via SQL.
/// Oracle: every intended update commits (retries allowed), exactly one row
/// remains visible, holding a value some writer wrote.
fn w_same_row(toggle: bool) -> Result<(), String> {
    let (_d, engine) = open_engine(toggle);
    seed_rows(&engine, true, 1);
    let writers = 6usize;
    let per = 25i64 * rounds_mult() as i64;
    let err: Shared = Arc::default();
    let committed = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(writers));
    let mut handles = Vec::new();
    for w in 0..writers {
        let engine = Arc::clone(&engine);
        let err = Arc::clone(&err);
        let committed = Arc::clone(&committed);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..per {
                let s = format!(
                    "UPDATE t SET k = {} WHERE id = 1",
                    1000 + w as i64 * per + i
                );
                match txn_retry(&engine, Isolation::ReadCommitted, &[s]) {
                    Ok(()) => {
                        committed.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(m) => {
                        note_err(&err, m);
                        return;
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().map_err(|_| "writer panicked".to_string())?;
    }
    take_err(&err)?;
    let done = committed.load(Ordering::SeqCst);
    if done != writers * per as usize {
        return Err(format!(
            "committed {done}/{} updates",
            writers * per as usize
        ));
    }
    let mut got = select_i64s(&engine, "SELECT id FROM t")?;
    assert_id_set(&mut got, &BTreeSet::from([1]), "post-contention scan")?;
    let k = select_i64s(&engine, "SELECT k FROM t")?;
    if k.len() != 1 || k[0] < 1000 {
        return Err(format!("final k {k:?} is not a written value"));
    }
    Ok(())
}

/// W4 — mixed INSERT/UPDATE/DELETE: each writer runs a deterministic script
/// over its own id range, so the expected final set is computable exactly.
fn w_mixed_crud(toggle: bool) -> Result<(), String> {
    let (_d, engine) = open_engine(toggle);
    ddl(
        &engine,
        &[
            "CREATE TABLE t (id INT, k INT)",
            "CREATE INDEX t_k ON t USING BTREE (k)",
        ],
    );
    let writers = 6usize;
    let per = 60i64 * rounds_mult() as i64;
    let err: Shared = Arc::default();
    let barrier = Arc::new(Barrier::new(writers));
    let mut handles = Vec::new();
    for w in 0..writers {
        let engine = Arc::clone(&engine);
        let err = Arc::clone(&err);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..per {
                let id = (w as i64) * 100_000 + i;
                let mut stmts = vec![format!("INSERT INTO t (id, k) VALUES ({id}, {})", id % 17)];
                match i % 3 {
                    1 => stmts.push(format!("UPDATE t SET k = {} WHERE id = {id}", id % 5)),
                    2 => stmts.push(format!("DELETE FROM t WHERE id = {id}")),
                    _ => {}
                }
                // Each statement its own txn: interleaves with other writers.
                for s in stmts {
                    if let Err(m) = txn_retry(&engine, Isolation::ReadCommitted, &[s]) {
                        note_err(&err, m);
                        return;
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().map_err(|_| "writer panicked".to_string())?;
    }
    take_err(&err)?;
    let expected: BTreeSet<i64> = (0..writers as i64)
        .flat_map(|w| {
            (0..per)
                .filter(|i| i % 3 != 2) // deleted every 3rd
                .map(move |i| w * 100_000 + i)
        })
        .collect();
    let mut got = select_i64s(&engine, "SELECT id FROM t")?;
    assert_id_set(&mut got, &expected, "post-mixed-CRUD scan")?;
    assert_index_agrees_with_scan(&engine, "t")
}

/// W5 — readers during churn: while writers churn cross-row UPDATEs (constant
/// logical row count), reader threads assert EVERY snapshot they open sees
/// exactly the seeded rows — scan, COUNT(*) (header-only fast path), and for
/// RR/SERIALIZABLE a second read in the same txn must repeat identically.
/// This is the live (in-flight) generalization of the item-16 oracle.
fn w_readers_during_churn(toggle: bool, iso: Isolation, parallel: bool) -> Result<(), String> {
    let (_d, engine) = open_engine(toggle);
    let rows: i64 = if parallel { 3_000 } else { 8 };
    seed_rows(&engine, true, rows);
    if parallel {
        // Force the parallel-scan path even at this modest page count.
        engine.set_parallel_scan(true);
        engine.set_parallel_scan_config(2, 4);
    }
    let writers = 4usize;
    let rounds = (if parallel { 30 } else { 80 }) * rounds_mult() as i64;
    let expected: BTreeSet<i64> = (1..=rows).collect();
    let err: Shared = Arc::default();
    let done = Arc::new(AtomicBool::new(false));

    let mut readers = Vec::new();
    for _ in 0..2 {
        let engine = Arc::clone(&engine);
        let err = Arc::clone(&err);
        let done = Arc::clone(&done);
        let expected = expected.clone();
        readers.push(thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let x = match engine.begin_with_isolation(iso) {
                    Ok(x) => x,
                    Err(e) => {
                        note_err(&err, format!("reader begin: {e:?}"));
                        return;
                    }
                };
                let step: Result<(), String> = (|| {
                    let mut ids = select_i64s_in(&engine, Some(x), "SELECT id FROM t")?;
                    assert_id_set(&mut ids, &expected, "reader scan")?;
                    let c = count_star(&engine, Some(x), "t")?;
                    if c != expected.len() as i64 {
                        return Err(format!("reader COUNT(*)={c}, expected {}", expected.len()));
                    }
                    if !matches!(iso, Isolation::ReadCommitted) {
                        let mut again = select_i64s_in(&engine, Some(x), "SELECT id FROM t")?;
                        assert_id_set(&mut again, &expected, "repeatable re-read")?;
                    }
                    Ok(())
                })();
                let _ = engine.commit(x);
                if let Err(m) = step {
                    note_err(&err, m);
                    return;
                }
            }
        }));
    }
    for h in churn_writers(&engine, writers, rows, rounds, &err) {
        h.join().map_err(|_| "writer panicked".to_string())?;
    }
    done.store(true, Ordering::Relaxed);
    for h in readers {
        h.join().map_err(|_| "reader panicked".to_string())?;
    }
    take_err(&err)?;
    let mut got = select_i64s(&engine, "SELECT id FROM t")?;
    assert_id_set(&mut got, &expected, "final scan")
}

/// W6 — balance transfers: writers move 1 unit between two accounts inside one
/// RR txn (read both balances, write both back). First-committer-wins aborts
/// the loser, so the total is invariant. Readers at ALL THREE isolation levels
/// assert every snapshot sums to the seeded total (client-side sum — no
/// dependence on the aggregate route).
fn w_transfer_sum(toggle: bool) -> Result<(), String> {
    let (_d, engine) = open_engine(toggle);
    let accounts: i64 = 8;
    let seed_balance: i64 = 100;
    let total = accounts * seed_balance;
    {
        let values: Vec<String> = (1..=accounts)
            .map(|i| format!("({i}, {seed_balance})"))
            .collect();
        ddl(
            &engine,
            &[
                "CREATE TABLE t (id INT, k INT)",
                "CREATE INDEX t_k ON t USING BTREE (k)",
                &format!("INSERT INTO t (id, k) VALUES {}", values.join(", ")),
            ],
        );
    }
    let writers = 6usize;
    let rounds = 40i64 * rounds_mult() as i64;
    let err: Shared = Arc::default();
    let done = Arc::new(AtomicBool::new(false));

    let mut readers = Vec::new();
    for iso in [
        Isolation::ReadCommitted,
        Isolation::RepeatableRead,
        Isolation::Serializable,
    ] {
        let engine = Arc::clone(&engine);
        let err = Arc::clone(&err);
        let done = Arc::clone(&done);
        readers.push(thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let x = match engine.begin_with_isolation(iso) {
                    Ok(x) => x,
                    Err(e) => {
                        note_err(&err, format!("reader begin: {e:?}"));
                        return;
                    }
                };
                let step: Result<(), String> = (|| {
                    let ks = select_i64s_in(&engine, Some(x), "SELECT k FROM t")?;
                    let sum: i64 = ks.iter().sum();
                    if ks.len() != accounts as usize || sum != total {
                        return Err(format!(
                            "snapshot ({iso:?}): {} accounts summing {sum}, expected {} @ {total}",
                            ks.len(),
                            accounts
                        ));
                    }
                    Ok(())
                })();
                let _ = engine.commit(x);
                if let Err(m) = step {
                    note_err(&err, m);
                    return;
                }
            }
        }));
    }

    let barrier = Arc::new(Barrier::new(writers));
    let mut handles = Vec::new();
    for w in 0..writers {
        let engine = Arc::clone(&engine);
        let err = Arc::clone(&err);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for r in 0..rounds {
                let from = (r + w as i64) % accounts + 1;
                let to = (r + w as i64 + 1) % accounts + 1;
                // Read-modify-write under RR: retry whole txn on conflict.
                let res: Result<(), String> = 'retry: loop {
                    let x = match engine.begin_with_isolation(Isolation::RepeatableRead) {
                        Ok(x) => x,
                        Err(e) => break 'retry Err(format!("begin: {e:?}")),
                    };
                    let step: Result<bool, String> = (|| {
                        let bf = select_i64s_in(
                            &engine,
                            Some(x),
                            &format!("SELECT k FROM t WHERE id = {from}"),
                        )?;
                        let bt = select_i64s_in(
                            &engine,
                            Some(x),
                            &format!("SELECT k FROM t WHERE id = {to}"),
                        )?;
                        let (bf, bt) = match (bf.first(), bt.first()) {
                            (Some(a), Some(b)) => (*a, *b),
                            _ => return Err("transfer read missed an account".into()),
                        };
                        for s in [
                            format!("UPDATE t SET k = {} WHERE id = {from}", bf - 1),
                            format!("UPDATE t SET k = {} WHERE id = {to}", bt + 1),
                        ] {
                            match engine.execute_sql(x, &s) {
                                Ok(_) => {}
                                Err(e) if retryable(&e) => return Ok(false),
                                Err(e) => return Err(format!("{s}: {e:?}")),
                            }
                        }
                        Ok(true)
                    })();
                    match step {
                        Ok(true) => match engine.commit(x) {
                            Ok(_) => break 'retry Ok(()),
                            Err(e) if retryable(&e) => continue,
                            Err(e) => break 'retry Err(format!("commit: {e:?}")),
                        },
                        Ok(false) => {
                            let _ = engine.abort(x);
                            continue;
                        }
                        Err(m) => {
                            let _ = engine.abort(x);
                            break 'retry Err(m);
                        }
                    }
                };
                if let Err(m) = res {
                    note_err(&err, m);
                    return;
                }
            }
        }));
    }
    for h in handles {
        h.join().map_err(|_| "writer panicked".to_string())?;
    }
    done.store(true, Ordering::Relaxed);
    for h in readers {
        h.join().map_err(|_| "reader panicked".to_string())?;
    }
    take_err(&err)?;
    let ks = select_i64s(&engine, "SELECT k FROM t")?;
    let sum: i64 = ks.iter().sum();
    if ks.len() != accounts as usize || sum != total {
        return Err(format!("final sum {sum}, expected {total}"));
    }
    Ok(())
}

/// W7 — vacuum interleaved with churn: a background thread vacuums continuously
/// while writers churn cross-row UPDATEs; oracle = the item-16 count invariant
/// plus index agreement (stale reclaimed slots must never resurface).
fn w_vacuum_churn(toggle: bool) -> Result<(), String> {
    let (_d, engine) = open_engine(toggle);
    let rows: i64 = 8;
    seed_rows(&engine, true, rows);
    let err: Shared = Arc::default();
    let done = Arc::new(AtomicBool::new(false));
    let vac = {
        let engine = Arc::clone(&engine);
        let done = Arc::clone(&done);
        thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let _ = engine.vacuum();
                thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let rounds = 60 * rounds_mult() as i64;
    for h in churn_writers(&engine, 4, rows, rounds, &err) {
        h.join().map_err(|_| "writer panicked".to_string())?;
    }
    done.store(true, Ordering::Relaxed);
    vac.join()
        .map_err(|_| "vacuum thread panicked".to_string())?;
    take_err(&err)?;
    engine
        .vacuum()
        .map_err(|e| format!("final vacuum: {e:?}"))?;

    let expected: BTreeSet<i64> = (1..=rows).collect();
    let mut got = select_i64s(&engine, "SELECT id FROM t")?;
    assert_id_set(&mut got, &expected, "post-vacuum scan")?;
    assert_index_agrees_with_scan(&engine, "t")
}

/// W8 — delete + reinsert (slot reuse/aliasing): each writer repeatedly deletes
/// and reinserts its own ids while readers assert no snapshot ever shows a
/// duplicate id or an id outside the universe. Final state: all ids present.
fn w_delete_reinsert(toggle: bool) -> Result<(), String> {
    let (_d, engine) = open_engine(toggle);
    ddl(
        &engine,
        &[
            "CREATE TABLE t (id INT, k INT)",
            "CREATE INDEX t_k ON t USING BTREE (k)",
        ],
    );
    let writers = 4usize;
    let per_writer_ids = 6i64;
    let rounds = 30i64 * rounds_mult() as i64;
    let universe: BTreeSet<i64> = (0..writers as i64)
        .flat_map(|w| (0..per_writer_ids).map(move |j| w * 1000 + j))
        .collect();
    // Seed the universe.
    {
        let values: Vec<String> = universe.iter().map(|id| format!("({id}, {id})")).collect();
        ddl(
            &engine,
            &[&format!(
                "INSERT INTO t (id, k) VALUES {}",
                values.join(", ")
            )],
        );
    }
    let err: Shared = Arc::default();
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let engine = Arc::clone(&engine);
        let err = Arc::clone(&err);
        let done = Arc::clone(&done);
        let universe = universe.clone();
        thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                match select_i64s(&engine, "SELECT id FROM t") {
                    Ok(mut ids) => {
                        ids.sort_unstable();
                        let n = ids.len();
                        ids.dedup();
                        if ids.len() != n {
                            note_err(&err, "duplicate id in one snapshot".into());
                            return;
                        }
                        if let Some(bad) = ids.iter().find(|i| !universe.contains(i)) {
                            note_err(&err, format!("id {bad} outside universe"));
                            return;
                        }
                    }
                    Err(m) => {
                        note_err(&err, m);
                        return;
                    }
                }
            }
        })
    };
    let barrier = Arc::new(Barrier::new(writers));
    let mut handles = Vec::new();
    for w in 0..writers {
        let engine = Arc::clone(&engine);
        let err = Arc::clone(&err);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for r in 0..rounds {
                let id = (w as i64) * 1000 + r % per_writer_ids;
                for s in [
                    format!("DELETE FROM t WHERE id = {id}"),
                    format!("INSERT INTO t (id, k) VALUES ({id}, {})", id + r),
                ] {
                    if let Err(m) = txn_retry(&engine, Isolation::ReadCommitted, &[s]) {
                        note_err(&err, m);
                        return;
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().map_err(|_| "writer panicked".to_string())?;
    }
    done.store(true, Ordering::Relaxed);
    reader.join().map_err(|_| "reader panicked".to_string())?;
    take_err(&err)?;
    let mut got = select_i64s(&engine, "SELECT id FROM t")?;
    assert_id_set(&mut got, &universe, "final scan")?;
    assert_index_agrees_with_scan(&engine, "t")
}

// ── matrix runner ────────────────────────────────────────────────────────────

struct Cell {
    id: &'static str,
    workload: &'static str,
    toggle: bool,
    index: &'static str,
    iso: &'static str,
    shape: String,
    run: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}

fn iso_label(iso: Isolation) -> &'static str {
    match iso {
        Isolation::ReadCommitted => "RC",
        Isolation::RepeatableRead => "RR",
        Isolation::Serializable => "SER",
    }
}

fn cells() -> Vec<Cell> {
    let mut v: Vec<Cell> = Vec::new();
    let onoff = [false, true];

    for &tg in &onoff {
        for &ix in &[false, true] {
            v.push(Cell {
                id: "insert-storm",
                workload: "8 writers × disjoint INSERTs, overlapping keys",
                toggle: tg,
                index: if ix { "btree(k)" } else { "none" },
                iso: "RC",
                shape: "8w".into(),
                run: Arc::new(move || w_insert_storm(tg, ix)),
            });
        }
    }
    // Cross-row churn — the item-16 shape. 2w×2rows is the exact failing-test
    // geometry; 8w×8rows widens it; the unindexed cells isolate the index path.
    for &tg in &onoff {
        for &(w, rows, ix) in &[(2usize, 2i64, true), (8, 8, true), (8, 8, false)] {
            v.push(Cell {
                id: "cross-row-churn",
                workload: "paired UPDATEs per txn, opposite lock order (item-16 shape)",
                toggle: tg,
                index: if ix { "btree(k)" } else { "none" },
                iso: "RC",
                shape: format!("{w}w × {rows}rows"),
                run: Arc::new(move || w_cross_row_churn(tg, ix, w, rows)),
            });
        }
    }
    for &tg in &onoff {
        v.push(Cell {
            id: "same-row-contention",
            workload: "6 writers UPDATE the same logical row",
            toggle: tg,
            index: "btree(k)",
            iso: "RC",
            shape: "6w × 1row".into(),
            run: Arc::new(move || w_same_row(tg)),
        });
        v.push(Cell {
            id: "mixed-crud",
            workload: "concurrent INSERT/UPDATE/DELETE, deterministic oracle",
            toggle: tg,
            index: "btree(k)",
            iso: "RC",
            shape: "6w".into(),
            run: Arc::new(move || w_mixed_crud(tg)),
        });
    }
    for &tg in &onoff {
        for iso in [
            Isolation::ReadCommitted,
            Isolation::RepeatableRead,
            Isolation::Serializable,
        ] {
            v.push(Cell {
                id: "readers-during-churn",
                workload: "2 readers assert row count + repeatable re-read mid-churn",
                toggle: tg,
                index: "btree(k)",
                iso: iso_label(iso),
                shape: "4w × 8rows + 2r".into(),
                run: Arc::new(move || w_readers_during_churn(tg, iso, false)),
            });
        }
        v.push(Cell {
            id: "parallel-scan-readers",
            workload: "readers on the parallel-worker scan path mid-churn",
            toggle: tg,
            index: "btree(k)",
            iso: "RC",
            shape: "4w × 3000rows + 2r".into(),
            run: Arc::new(move || w_readers_during_churn(tg, Isolation::ReadCommitted, true)),
        });
        v.push(Cell {
            id: "transfer-sum",
            workload: "RR read-modify-write transfers; readers at RC/RR/SER sum invariant",
            toggle: tg,
            index: "btree(k)",
            iso: "RC+RR+SER",
            shape: "6w × 8accts + 3r".into(),
            run: Arc::new(move || w_transfer_sum(tg)),
        });
        v.push(Cell {
            id: "vacuum-churn",
            workload: "continuous vacuum racing cross-row UPDATE churn",
            toggle: tg,
            index: "btree(k)",
            iso: "RC",
            shape: "4w × 8rows + vac".into(),
            run: Arc::new(move || w_vacuum_churn(tg)),
        });
        v.push(Cell {
            id: "delete-reinsert",
            workload: "DELETE + re-INSERT churn (slot reuse); reader dup/alias check",
            toggle: tg,
            index: "btree(k)",
            iso: "RC",
            shape: "4w × 24ids + 1r".into(),
            run: Arc::new(move || w_delete_reinsert(tg)),
        });
    }
    v
}

/// Run one repeat on a detached thread with a hang deadline: a deadlock/
/// livelock becomes a FAIL row and the matrix CONTINUES with the next cell.
/// The hung thread (and its tempdir engine) is deliberately abandoned — it
/// holds only its own resources, and any residual CPU it burns just adds to
/// the contention the matrix wants anyway. A hang skips the cell's remaining
/// repeats (they would almost certainly hang too, at `secs` each).
fn run_with_deadline(cell: &Cell, secs: u64) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let run = Arc::clone(&cell.run);
    thread::spawn(move || {
        let res =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run())).unwrap_or_else(|p| {
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".into());
                Err(format!("panic: {msg}"))
            });
        let _ = tx.send(res);
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(res) => res,
        Err(_) => Err(format!(
            "HANG: exceeded the {secs}s deadline (deadlock/livelock) — worker abandoned"
        )),
    }
}

fn main() {
    let only = std::env::var("CONC_ONLY").unwrap_or_default();
    let reps = repeats();
    let ncpu = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    let spinners = env_usize("CONC_SPIN", ncpu);

    // CPU-contention amplifier: the item-16 anomaly only fires under scheduler
    // pressure (it never reproduced in an idle run), so oversubscribe the cores
    // for the whole matrix.
    let spin_stop = Arc::new(AtomicBool::new(false));
    let mut spin_handles = Vec::new();
    for _ in 0..spinners {
        let stop = Arc::clone(&spin_stop);
        spin_handles.push(thread::spawn(move || {
            let mut x = 0u64;
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..10_000 {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                }
                std::hint::black_box(x);
            }
        }));
    }

    println!("## Concurrency correctness matrix (border cases)");
    println!();
    println!(
        "_Every cell is a **correctness** check, not a throughput number: a \
         production-shaped concurrent read/write workload runs to completion and \
         its invariant oracle is asserted (row-count/visibility, no duplicate ids \
         in any snapshot, repeatable re-reads, sum invariance, index-vs-scan \
         agreement). A cell FAILs if **any** repeat violates its oracle._"
    );
    println!();
    println!(
        "- toggle = `UNIDB_CONCURRENT_SQL_WRITES` (**on is the production \
         default** as of the item-11 flip; off forces the serialized fallback)"
    );
    println!(
        "- CPU contention: {spinners} spinner threads oversubscribing {ncpu} cores \
         for the whole run (`CONC_SPIN`) — the item-16 anomaly needs scheduler \
         pressure to fire"
    );
    println!(
        "- repeats/cell: {reps} (`CONC_REPEATS`); workload multiplier: {}× (`CONC_ROUNDS`)",
        rounds_mult()
    );
    println!();
    println!("| # | scenario | toggle | index | reader iso | shape | result | detail |");
    println!("|---|----------|--------|-------|------------|-------|--------|--------|");

    let mut n_pass = 0usize;
    let mut n_fail = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let deadline = deadline_secs();

    for (i, cell) in cells().iter().enumerate() {
        if !only.is_empty() && !cell.id.contains(&only) {
            continue;
        }
        let mut fail_msgs: Vec<String> = Vec::new();
        for _ in 0..reps {
            if let Err(m) = run_with_deadline(cell, deadline) {
                let is_hang = m.starts_with("HANG");
                fail_msgs.push(m);
                if is_hang {
                    break; // remaining repeats would hang too, at `deadline` each
                }
            }
        }
        let toggle = if cell.toggle { "on" } else { "**off**" };
        let (result, detail) = if fail_msgs.is_empty() {
            n_pass += 1;
            ("PASS".to_string(), format!("{reps}/{reps} repeats clean"))
        } else {
            n_fail += 1;
            let msg = fail_msgs[0].replace('|', "\\|");
            failures.push(format!(
                "`{}` (toggle {}, {}): {} — {}/{} repeats failed",
                cell.id,
                if cell.toggle { "on" } else { "off" },
                cell.shape,
                msg,
                fail_msgs.len(),
                reps
            ));
            (
                format!("**FAIL {}/{reps}**", fail_msgs.len()),
                msg.chars().take(120).collect(),
            )
        };
        println!(
            "| {} | {} — {} | {} | {} | {} | {} | {} | {} |",
            i + 1,
            cell.id,
            cell.workload,
            toggle,
            cell.index,
            cell.iso,
            cell.shape,
            result,
            detail
        );
    }

    spin_stop.store(true, Ordering::Relaxed);
    for h in spin_handles {
        let _ = h.join();
    }

    println!();
    println!("**Summary: {n_pass} PASS · {n_fail} FAIL.**");
    println!();
    if n_fail > 0 {
        println!("Failing cells (first violation each):");
        println!();
        for f in &failures {
            println!("- {f}");
        }
        println!();
    }
    println!(
        "_How to read this: rows with toggle **on** are the shipping production \
         default (as of item 11's default-ON flip) — any FAIL there is a release \
         blocker. Rows with toggle *off* exercise the serialized `cat_write` \
         fallback (`UNIDB_CONCURRENT_SQL_WRITES=0`), the residual-race revert \
         path. The item-16 MVCC visibility anomaly this matrix was \
         built to catch (abort dropped the xid from `active` before undo) was \
         root-caused and FIXED 2026-07-12 — see \
         `docs/backlog/16_concurrent_sql_writes_visibility_anomaly.md`; these \
         cells now stand as its permanent regression gate. Intermittency \
         caveat: a PASS is evidence, not proof — raise `CONC_REPEATS` (and run \
         alongside other load) to tighten the net._"
    );

    if n_fail > 0 && std::env::var("CONC_STRICT").as_deref() == Ok("1") {
        std::process::exit(1);
    }
}
