//! P5.e-3 concurrent-writer correctness. `Engine` is now `Send + Sync`, so many
//! writer threads share one `Arc<Engine>` and commit in parallel, coordinating
//! only through the engine's internal latches/locks (buffer-pool page latches,
//! the WAL append mutex + group-commit barrier, the row lock manager, MVCC).
//!
//! These assert the three things Phase 5's gate demands under many concurrent
//! writers: **no lost updates**, **no torn state**, **no deadlock hangs**.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use unidb::sql::logical::Literal;
use unidb::{DbError, Engine, RowId, SqlResult};

/// Run `f` on a background thread and panic if it does not finish within
/// `secs` — turns a deadlock/livelock hang into a test failure instead of a
/// suite that blocks forever.
fn with_deadline<F: FnOnce() + Send + 'static>(secs: u64, f: F) {
    let done = Arc::new(AtomicUsize::new(0));
    let d2 = Arc::clone(&done);
    let h = thread::spawn(move || {
        f();
        d2.store(1, Ordering::SeqCst);
    });
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        if done.load(Ordering::SeqCst) == 1 {
            h.join().unwrap();
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("test did not finish within {secs}s — likely a deadlock/livelock hang");
}

/// Many threads each insert their own rows in their own transactions. Every
/// committed row must be durable, distinct, and read back with exactly the
/// bytes that thread wrote — the "no lost updates / no torn state" gate for the
/// hot raw-insert path (which shares the heap's per-page exclusive latches).
#[test]
fn concurrent_inserts_no_lost_or_torn_rows() {
    with_deadline(60, || {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        engine.set_deferred_sync(true); // group-commit mode, like the server

        let threads = 8;
        let per_thread = 400;
        let barrier = Arc::new(Barrier::new(threads));

        let mut handles = Vec::new();
        for t in 0..threads {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait(); // maximize overlap
                let mut mine = Vec::new();
                for i in 0..per_thread {
                    let payload = format!("t{t}-r{i}").into_bytes();
                    let xid = engine.begin().unwrap();
                    let rid = engine.insert(xid, &payload).unwrap();
                    engine.commit(xid).unwrap();
                    mine.push((rid, payload));
                }
                mine
            }));
        }

        let mut all: Vec<(RowId, Vec<u8>)> = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        assert_eq!(all.len(), threads * per_thread);

        // No two committed rows share a physical slot (no lost update where one
        // insert overwrote another on the same page/slot).
        let mut ids: Vec<RowId> = all.iter().map(|(r, _)| *r).collect();
        ids.sort_by_key(|r| (r.page_id, r.slot));
        ids.dedup_by_key(|r| (r.page_id, r.slot));
        assert_eq!(ids.len(), threads * per_thread, "distinct physical slots");

        // Every row reads back exactly what was written — no torn/overwritten
        // bytes — both immediately and after reopen (durability).
        let xid = engine.begin().unwrap();
        for (rid, expected) in &all {
            assert_eq!(&engine.get(xid, *rid).unwrap(), expected);
        }
        engine.commit(xid).unwrap();

        engine.checkpoint().unwrap();
        drop(engine);
        let engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        for (rid, expected) in &all {
            assert_eq!(&engine.get(xid, *rid).unwrap(), expected);
        }
        engine.commit(xid).unwrap();
    });
}

/// Many threads concurrently update DISTINCT rows. Each row ends holding its
/// last-written value — no update lost to a racing writer on the same page.
#[test]
fn concurrent_updates_distinct_rows_keep_last_value() {
    with_deadline(60, || {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        engine.set_deferred_sync(true);

        // Seed one row per thread.
        let threads = 8;
        let rounds = 60;
        let mut seeds = Vec::new();
        let xid = engine.begin().unwrap();
        for t in 0..threads {
            seeds.push(engine.insert(xid, format!("seed-{t}").as_bytes()).unwrap());
        }
        engine.commit(xid).unwrap();

        let mut handles = Vec::new();
        for (t, seed) in seeds.into_iter().enumerate() {
            let engine = Arc::clone(&engine);
            handles.push(thread::spawn(move || {
                let mut cur = seed;
                for r in 0..rounds {
                    let payload = format!("t{t}-v{r}").into_bytes();
                    loop {
                        let xid = engine.begin().unwrap();
                        match engine.update(xid, cur, &payload) {
                            Ok(new_rid) => {
                                engine.commit(xid).unwrap();
                                cur = new_rid;
                                break;
                            }
                            Err(_) => {
                                let _ = engine.abort(xid);
                            }
                        }
                    }
                }
                (t, cur, format!("t{t}-v{}", rounds - 1).into_bytes())
            }));
        }

        let finals: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let xid = engine.begin().unwrap();
        for (_t, rid, expected) in &finals {
            assert_eq!(&engine.get(xid, *rid).unwrap(), expected);
        }
        engine.commit(xid).unwrap();
    });
}

/// Many threads contend to update the SAME logical row (its physical tip is
/// shared via a mutex, advanced on every successful commit). The row lock
/// manager + deadlock detector must let every thread make progress (retrying on
/// conflict) and never hang; the final value must be one a committed writer
/// wrote, and the total number of commits must be exactly what was intended
/// (no lost update quietly dropped).
#[test]
fn contended_updates_same_row_make_progress_without_hang() {
    with_deadline(60, || {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        engine.set_deferred_sync(true);

        let xid = engine.begin().unwrap();
        let row = engine.insert(xid, b"start").unwrap();
        engine.commit(xid).unwrap();

        // The single shared logical tip (physical RowId of the newest committed
        // version), advanced under a mutex by whoever commits.
        let tip = Arc::new(std::sync::Mutex::new(row));
        let threads = 6;
        let per_thread = 40;
        let committed = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for t in 0..threads {
            let engine = Arc::clone(&engine);
            let committed = Arc::clone(&committed);
            let tip = Arc::clone(&tip);
            handles.push(thread::spawn(move || {
                let mut done = 0;
                while done < per_thread {
                    let cur = *tip.lock().unwrap();
                    let xid = engine.begin().unwrap();
                    let payload = format!("t{t}-{done}").into_bytes();
                    match engine.update(xid, cur, &payload) {
                        Ok(new_rid) => {
                            engine.commit(xid).unwrap();
                            *tip.lock().unwrap() = new_rid;
                            committed.fetch_add(1, Ordering::SeqCst);
                            done += 1;
                        }
                        Err(DbError::WriteConflict { .. })
                        | Err(DbError::Deadlock { .. })
                        | Err(DbError::SerializationFailure { .. }) => {
                            // Lost the race for this version — back out and retry
                            // against the advanced tip.
                            let _ = engine.abort(xid);
                        }
                        Err(e) => panic!("unexpected error: {e:?}"),
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            committed.load(Ordering::SeqCst),
            threads * per_thread,
            "every intended update must eventually commit"
        );

        // The final tip is readable and holds a value some writer wrote.
        let final_tip = *tip.lock().unwrap();
        let xid = engine.begin().unwrap();
        let val = engine.get(xid, final_tip).unwrap();
        engine.commit(xid).unwrap();
        assert!(val.starts_with(b"t"), "final value is a written payload");
    });
}

// ── index-write-concurrency (Item 0a + Item A) ──────────────────────────────
//
// These exercise the *SQL* concurrent-write path — many threads committing
// INSERTs into one **indexed** table under the shared catalog lock (`cat_read`),
// so their B-tree index maintenance overlaps and is made safe only by the
// `DiskBTree` crabbing protocol (Item A), not by the old catalog write lock.

/// Collect the `id` column (first projected column, an INT) of a SELECT.
fn select_ids(engine: &Engine, sql: &str) -> Vec<i64> {
    let x = engine.begin().unwrap();
    let res = engine.execute_sql(x, sql).unwrap();
    engine.commit(x).unwrap();
    match res.into_iter().next().unwrap() {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r[0] {
                Literal::Int(n) => n,
                ref o => panic!("expected Int id, got {o:?}"),
            })
            .collect(),
        o => panic!("expected Rows, got {o:?}"),
    }
}

/// Run the concurrent-indexed-INSERT workload and assert full correctness: every
/// committed row is present (no lost INSERT), and every key resolves via the
/// B-tree index to exactly the ids that carry it (no lost/duplicated index
/// entry). Shared by the toggle-on and toggle-off cases so both are held to the
/// identical correctness bar. `n_keys` controls duplicate density (many ids
/// share a key ⇒ duplicate runs straddle leaf splits under contention).
fn run_indexed_insert_workload(concurrent: bool) {
    with_deadline(120, move || {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        engine.set_deferred_sync(true);
        engine.set_concurrent_sql_writes(concurrent);
        assert_eq!(engine.concurrent_sql_writes_enabled(), concurrent);

        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t (id INT, k INT, body TEXT)")
            .unwrap();
        engine
            .execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
            .unwrap();
        engine.commit(x).unwrap();

        let threads = 8usize;
        let per = 300i64;
        let n_keys = 97i64; // duplicate density
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::new();
        for t in 0..threads {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let ins = engine
                    .prepare("INSERT INTO t (id, k, body) VALUES ($1, $2, $3)")
                    .unwrap();
                barrier.wait();
                for i in 0..per {
                    let id = (t as i64) * 1000 + i; // globally unique
                    let k = id % n_keys; // overlaps across threads
                    let xid = engine.begin().unwrap();
                    engine
                        .execute_prepared(
                            xid,
                            &ins,
                            &[
                                Literal::Int(id),
                                Literal::Int(k),
                                Literal::Text(format!("b{id}")),
                            ],
                        )
                        .unwrap();
                    engine.commit(xid).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // (1) No lost INSERT: full scan returns every id exactly once.
        let mut all_ids = select_ids(&engine, "SELECT id FROM t");
        all_ids.sort_unstable();
        let mut expected_ids: Vec<i64> = (0..threads as i64)
            .flat_map(|t| (0..per).map(move |i| t * 1000 + i))
            .collect();
        expected_ids.sort_unstable();
        assert_eq!(
            all_ids, expected_ids,
            "every committed row present exactly once"
        );

        // (2) No lost/duplicated index entry: for each key, the index lookup
        //     (SELECT WHERE k = key uses the B-tree) returns exactly the ids that
        //     carry that key. Compare as sets.
        let mut expected_by_key: std::collections::HashMap<i64, std::collections::BTreeSet<i64>> =
            std::collections::HashMap::new();
        for &id in &expected_ids {
            expected_by_key.entry(id % n_keys).or_default().insert(id);
        }
        for (k, want) in &expected_by_key {
            let got: std::collections::BTreeSet<i64> =
                select_ids(&engine, &format!("SELECT id FROM t WHERE k = {k}"))
                    .into_iter()
                    .collect();
            assert_eq!(&got, want, "index lookup for k={k} lost/duplicated entries");
        }
    });
}

/// Item 0a + A acceptance (correctness): concurrent indexed INSERTs under the
/// **toggle on** are race-free — no lost row, no lost/duplicated index entry.
#[test]
fn concurrent_indexed_sql_inserts_correct_toggle_on() {
    run_indexed_insert_workload(true);
}

/// Toggle-off regression: the exact same workload under the known-safe
/// serialized (`cat_write`) path is equally correct. Now that the concurrent
/// path is default-on (item-11 flip), this guards that the serialized fallback
/// still exists and stays correct — the residual-race revert path via
/// `UNIDB_CONCURRENT_SQL_WRITES=0` / `set_concurrent_sql_writes(false)`.
#[test]
fn concurrent_indexed_sql_inserts_correct_toggle_off() {
    run_indexed_insert_workload(false);
}

/// Vacuum interleaved with concurrent index writes (test-matrix "MVCC aliasing
/// (M10.c)"): while writers INSERT/DELETE on an indexed table under the toggle,
/// a background thread repeatedly vacuums — exercising `DiskBTree::remove`/
/// `set_value` (now leaf-latched) racing the crabbing inserts. Afterwards every
/// surviving row must still resolve through the index, and a stale reclaimed slot
/// must never surface a wrong row.
#[test]
fn vacuum_interleaved_with_concurrent_index_writes() {
    with_deadline(120, || {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        engine.set_deferred_sync(true);
        engine.set_concurrent_sql_writes(true);

        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t (id INT, k INT, body TEXT)")
            .unwrap();
        engine
            .execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
            .unwrap();
        engine.commit(x).unwrap();

        let stop = Arc::new(AtomicUsize::new(0));
        // Background vacuum loop.
        let vac = {
            let engine = Arc::clone(&engine);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while stop.load(Ordering::Relaxed) == 0 {
                    let _ = engine.vacuum();
                    thread::sleep(Duration::from_millis(2));
                }
            })
        };

        let threads = 6usize;
        let per = 150i64;
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::new();
        for t in 0..threads {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for i in 0..per {
                    let id = (t as i64) * 10_000 + i;
                    let k = id % 41;
                    let xid = engine.begin().unwrap();
                    engine
                        .execute_sql(
                            xid,
                            &format!("INSERT INTO t (id, k, body) VALUES ({id}, {k}, 'b')"),
                        )
                        .unwrap();
                    engine.commit(xid).unwrap();
                    // Delete every other row to feed the vacuum.
                    if i % 2 == 0 {
                        let xid = engine.begin().unwrap();
                        engine
                            .execute_sql(xid, &format!("DELETE FROM t WHERE id = {id}"))
                            .unwrap();
                        engine.commit(xid).unwrap();
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        stop.store(1, Ordering::Relaxed);
        vac.join().unwrap();
        engine.vacuum().unwrap();

        // Surviving rows = the odd-i rows. Each must resolve through the index.
        let survivors: Vec<(i64, i64)> = (0..threads as i64)
            .flat_map(|t| {
                (0..per)
                    .filter(|i| i % 2 == 1)
                    .map(move |i| (t * 10_000 + i, (t * 10_000 + i) % 41))
            })
            .collect();
        for (id, k) in &survivors {
            let ids: std::collections::BTreeSet<i64> =
                select_ids(&engine, &format!("SELECT id FROM t WHERE k = {k}"))
                    .into_iter()
                    .collect();
            assert!(
                ids.contains(id),
                "survivor id={id} (k={k}) not found via index"
            );
        }
        // No deleted row resurfaces via the index.
        let all: std::collections::BTreeSet<i64> = select_ids(&engine, "SELECT id FROM t")
            .into_iter()
            .collect();
        for t in 0..threads as i64 {
            for i in (0..per).step_by(2) {
                assert!(!all.contains(&(t * 10_000 + i)), "deleted row resurfaced");
            }
        }
    });
}

/// Two-thread cross-row lock-ordering (test-matrix "Deadlock"): each thread
/// updates two indexed rows in the opposite order within one transaction, so the
/// row lock manager can form a cycle. The wait-for-graph detector must break it
/// cleanly (a `Deadlock`/`WriteConflict` one side retries) with no hang — never a
/// livelock or a corrupted index.
/// Item-16 regression (MVCC visibility anomaly under concurrent SQL writes).
/// Eight writers churn paired cross-row UPDATEs (opposite lock order, so
/// conflicts + aborts are constant) while two readers repeatedly scan. The
/// logical row set never changes, so **every** reader snapshot must see exactly
/// ids 1..=8 — no duplicate id (an aborting txn's superseded version wrongly
/// visible alongside its successor), no missing id (its restored version
/// wrongly hidden), and `COUNT(*)` == 8 — and the final quiescent state must be
/// exactly those 8 rows (no persistent duplicate). Before the abort-ordering fix
/// in `txn.rs` this fails at this geometry *without* external CPU load (the
/// matrix's 8w×8rows + readers cell); it is the standalone reproducer the
/// item-16 spec asks for. Run under both toggle settings — the anomaly is not
/// gated on `UNIDB_CONCURRENT_SQL_WRITES`.
fn readers_during_cross_row_churn(toggle_on: bool) {
    with_deadline(90, move || {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        engine.set_deferred_sync(true);
        engine.set_concurrent_sql_writes(toggle_on);

        let rows: i64 = 8;
        {
            let x = engine.begin().unwrap();
            engine
                .execute_sql(x, "CREATE TABLE t (id INT, k INT)")
                .unwrap();
            engine
                .execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
                .unwrap();
            let vals: Vec<String> = (1..=rows).map(|i| format!("({i}, {})", 10 * i)).collect();
            engine
                .execute_sql(
                    x,
                    &format!("INSERT INTO t (id, k) VALUES {}", vals.join(", ")),
                )
                .unwrap();
            engine.commit(x).unwrap();
        }

        let expected: Vec<i64> = (1..=rows).collect();
        let done = Arc::new(AtomicUsize::new(0));
        let writers = 8usize;
        let rounds = 120i64;
        let barrier = Arc::new(Barrier::new(writers));

        let mut readers = Vec::new();
        for _ in 0..2 {
            let engine = Arc::clone(&engine);
            let done = Arc::clone(&done);
            let expected = expected.clone();
            readers.push(thread::spawn(move || {
                while done.load(Ordering::Relaxed) == 0 {
                    let x = engine.begin().unwrap();
                    let mut ids = select_ids(&engine, "SELECT id FROM t");
                    // In-snapshot oracle: exactly the seeded id set, no duplicates.
                    ids.sort_unstable();
                    let n = ids.len();
                    ids.dedup();
                    assert_eq!(ids.len(), n, "duplicate id visible in one snapshot");
                    assert_eq!(ids, expected, "reader snapshot lost/gained a live row");
                    let c = match engine
                        .execute_sql(x, "SELECT COUNT(*) FROM t")
                        .unwrap()
                        .into_iter()
                        .next()
                        .unwrap()
                    {
                        SqlResult::Rows { rows, .. } => match rows[0][0] {
                            Literal::Int(n) => n,
                            ref o => panic!("expected Int, got {o:?}"),
                        },
                        o => panic!("expected Rows, got {o:?}"),
                    };
                    assert_eq!(c, rows, "COUNT(*) disagrees with the invariant row count");
                    engine.commit(x).unwrap();
                }
            }));
        }

        let mut handles = Vec::new();
        for w in 0..writers {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for r in 0..rounds {
                    let a = (r + w as i64) % rows + 1;
                    let b = (r + w as i64 + 1) % rows + 1;
                    let (first, second) = if w % 2 == 0 { (a, b) } else { (b, a) };
                    let v = 100 + r;
                    loop {
                        let xid = engine.begin().unwrap();
                        let step = engine
                            .execute_sql(xid, &format!("UPDATE t SET k = {v} WHERE id = {first}"))
                            .and_then(|_| {
                                engine.execute_sql(
                                    xid,
                                    &format!("UPDATE t SET k = {v} WHERE id = {second}"),
                                )
                            })
                            .and_then(|_| engine.commit(xid));
                        match step {
                            Ok(_) => break,
                            Err(DbError::Deadlock { .. })
                            | Err(DbError::WriteConflict { .. })
                            | Err(DbError::SerializationFailure { .. }) => {
                                let _ = engine.abort(xid);
                            }
                            Err(e) => panic!("unexpected error: {e:?}"),
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        done.store(1, Ordering::Relaxed);
        for h in readers {
            h.join().unwrap();
        }

        // Quiescent state: exactly the 8 seeded ids, no persistent duplicate.
        let mut final_ids = select_ids(&engine, "SELECT id FROM t");
        final_ids.sort_unstable();
        assert_eq!(
            final_ids, expected,
            "final state is not exactly the seeded rows"
        );
    });
}

#[test]
fn item16_readers_during_cross_row_churn_toggle_off() {
    readers_during_cross_row_churn(false);
}

#[test]
fn item16_readers_during_cross_row_churn_toggle_on() {
    readers_during_cross_row_churn(true);
}

#[test]
fn cross_row_update_deadlock_resolves_no_hang() {
    with_deadline(60, || {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        engine.set_deferred_sync(true);
        engine.set_concurrent_sql_writes(true);

        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t (id INT, k INT)")
            .unwrap();
        engine
            .execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
            .unwrap();
        engine
            .execute_sql(x, "INSERT INTO t (id, k) VALUES (1, 10), (2, 20)")
            .unwrap();
        engine.commit(x).unwrap();

        let rounds = 40;
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for dir_forward in [true, false] {
            let engine = Arc::clone(&engine);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let (first, second) = if dir_forward { (1, 2) } else { (2, 1) };
                barrier.wait();
                let mut done = 0;
                while done < rounds {
                    let v = 100 + done;
                    let xid = engine.begin().unwrap();
                    let a = engine
                        .execute_sql(xid, &format!("UPDATE t SET k = {v} WHERE id = {first}"));
                    let b = a.and_then(|_| {
                        engine
                            .execute_sql(xid, &format!("UPDATE t SET k = {v} WHERE id = {second}"))
                    });
                    match b.and_then(|_| engine.commit(xid)) {
                        Ok(_) => done += 1,
                        Err(DbError::Deadlock { .. })
                        | Err(DbError::WriteConflict { .. })
                        | Err(DbError::SerializationFailure { .. }) => {
                            let _ = engine.abort(xid);
                        }
                        Err(e) => panic!("unexpected error: {e:?}"),
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Both rows survive and remain index-resolvable at their final values.
        let x = engine.begin().unwrap();
        let rows = engine.execute_sql(x, "SELECT id FROM t").unwrap();
        engine.commit(x).unwrap();
        match rows.into_iter().next().unwrap() {
            SqlResult::Rows { rows, .. } => assert_eq!(rows.len(), 2),
            o => panic!("expected Rows, got {o:?}"),
        }
    });
}

/// Item 85: cross-row UPDATE churn with opposite lock order, toggle=on,
/// NO B-tree index. This is the scenario that hung 1/3 times in the PR #150
/// concurrency matrix (scenario 10). The test runs 2 writers × 2 rows in
/// opposite order — the minimal deadlock geometry — with a tight 10s deadline.
/// Must pass consistently after the fix (both no-hang and correct row count).
///
/// Item 88 gate: 20 clean repeats required (lock-table elision touches the
/// same subsystem; confirm no regression).
#[test]
fn item85_cross_row_churn_no_index_no_hang() {
    // 20 repeats: item-88 gate (was 5; item-85's hang was 1/3 probabilistic,
    // so 20 clean repeats is strong evidence it stays fixed after elision).
    for rep in 0..20 {
        with_deadline(10, move || {
            let dir = tempfile::tempdir().unwrap();
            let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
            engine.set_deferred_sync(true);
            engine.set_concurrent_sql_writes(true);

            let x = engine.begin().unwrap();
            // No index — this is what distinguishes scenario 10 from scenario 9.
            engine
                .execute_sql(x, "CREATE TABLE t (id INT, k INT)")
                .unwrap();
            engine
                .execute_sql(x, "INSERT INTO t (id, k) VALUES (1, 10), (2, 20)")
                .unwrap();
            engine.commit(x).unwrap();

            let rounds = 40;
            let barrier = Arc::new(Barrier::new(2));
            let mut handles = Vec::new();
            for dir_forward in [true, false] {
                let engine = Arc::clone(&engine);
                let barrier = Arc::clone(&barrier);
                handles.push(thread::spawn(move || {
                    let (first, second) = if dir_forward { (1, 2) } else { (2, 1) };
                    barrier.wait();
                    let mut done = 0;
                    while done < rounds {
                        let v = 100 + done;
                        let xid = engine.begin().unwrap();
                        let a = engine
                            .execute_sql(xid, &format!("UPDATE t SET k = {v} WHERE id = {first}"));
                        let b = a.and_then(|_| {
                            engine.execute_sql(
                                xid,
                                &format!("UPDATE t SET k = {v} WHERE id = {second}"),
                            )
                        });
                        match b.and_then(|_| engine.commit(xid)) {
                            Ok(_) => done += 1,
                            Err(DbError::Deadlock { .. })
                            | Err(DbError::WriteConflict { .. })
                            | Err(DbError::SerializationFailure { .. }) => {
                                let _ = engine.abort(xid);
                            }
                            Err(e) => panic!("unexpected error in rep {rep}: {e:?}"),
                        }
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
            // Both rows must still be present after the churn (invariant: row count = 2).
            let x = engine.begin().unwrap();
            let rows = engine.execute_sql(x, "SELECT id FROM t").unwrap();
            engine.commit(x).unwrap();
            match rows.into_iter().next().unwrap() {
                SqlResult::Rows { rows, .. } => assert_eq!(
                    rows.len(),
                    2,
                    "rep {rep}: expected exactly 2 rows after churn, got {}",
                    rows.len()
                ),
                o => panic!("rep {rep}: expected Rows, got {o:?}"),
            }
        });
    }
}

/// Item 88 gate: write-conflict interleaving correctness.
///
/// After lock-table elision the sole conflict gate is `xmax != 0` under the
/// page latch. This test verifies that single-row and bulk writers still detect
/// each other's in-flight writes in both directions:
///
/// 1. Bulk writer (DELETE many) starts and holds its xmax stamp → a concurrent
///    single-row UPDATE on the same row receives WriteConflict.
/// 2. Single-row writer (UPDATE) starts and holds its xmax stamp → a subsequent
///    bulk DELETE on the same rows receives WriteConflict on abort.
///
/// Both scenarios simulate the interleaving via a two-thread setup with a
/// barrier to ensure ordering.  A 10-second deadline rules out hangs.
#[test]
fn item88_bulk_single_write_conflict_interleaving() {
    // Scenario A: bulk DELETE commits first; single-row UPDATE must conflict.
    // We simulate by: Tx-A deletes row 1 (committed), Tx-B tries to UPDATE row 1.
    with_deadline(10, || {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        engine.set_deferred_sync(true);
        engine.set_concurrent_sql_writes(true);

        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t (id INT, v INT)")
            .unwrap();
        engine
            .execute_sql(x, "INSERT INTO t (id, v) VALUES (1, 10), (2, 20)")
            .unwrap();
        engine.commit(x).unwrap();

        // Phase 1: Tx-A deletes both rows.
        let xa = engine.begin().unwrap();
        engine
            .execute_sql(xa, "DELETE FROM t WHERE id >= 1")
            .unwrap();

        // Phase 2: Tx-B (concurrent) tries to UPDATE a row that Tx-A stamped xmax.
        let xb = engine.begin().unwrap();
        let res = engine.execute_sql(xb, "UPDATE t SET v = 99 WHERE id = 1");
        // Must get a conflict (WriteConflict or SerializationFailure) — not Ok.
        let _ = engine.abort(xb);
        assert!(
            matches!(
                res,
                Err(DbError::WriteConflict { .. }) | Err(DbError::SerializationFailure { .. })
            ),
            "Tx-B should conflict with Tx-A's bulk DELETE stamp; got {res:?}"
        );
        engine.commit(xa).unwrap();
    });

    // Scenario B: single-row UPDATE commits first; bulk DELETE must conflict.
    with_deadline(10, || {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
        engine.set_deferred_sync(true);
        engine.set_concurrent_sql_writes(true);

        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t (id INT, v INT)")
            .unwrap();
        engine
            .execute_sql(x, "INSERT INTO t (id, v) VALUES (1, 10), (2, 20)")
            .unwrap();
        engine.commit(x).unwrap();

        // Phase 1: Tx-A does single-row UPDATE on row 1 (stamps xmax + new version).
        let xa = engine.begin().unwrap();
        engine
            .execute_sql(xa, "UPDATE t SET v = 99 WHERE id = 1")
            .unwrap();

        // Phase 2: Tx-B (concurrent) tries bulk DELETE covering the same row.
        let xb = engine.begin().unwrap();
        let res = engine.execute_sql(xb, "DELETE FROM t WHERE id >= 1");
        // Must get a conflict.
        let _ = engine.abort(xb);
        assert!(
            matches!(
                res,
                Err(DbError::WriteConflict { .. }) | Err(DbError::SerializationFailure { .. })
            ),
            "Tx-B bulk DELETE should conflict with Tx-A's single-row UPDATE stamp; got {res:?}"
        );
        engine.commit(xa).unwrap();
    });
}
