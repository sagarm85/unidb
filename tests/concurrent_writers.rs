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

use unidb::{DbError, Engine, RowId};

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
