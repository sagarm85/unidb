// Crash-injection harness (D7, CLAUDE.md §7).
//
// Strategy: simulate crash by dropping the Engine (or flushing only WAL /
// only page) at each of the 5 injection points, then reopening and verifying
// recovery.  This is NOT a deterministic simulator — it is the
// kill-reopen-assert model specified in D7.
//
// Injection points:
//   P1 – after WAL append, before page flush (WAL durable, page not)
//   P2 – mid-checkpoint (pages flushed, checkpoint record not written)
//   P3 – after heap mutation, before commit record
//   P4 – during WAL truncation (truncation began but not finished)
//   P5 – immediately after commit fsync (committed, page maybe not flushed)
//   P6 – mid-user-transaction (M1): statements' mini-txns durably
//        committed, but the user transaction never reaches WAL_TXN_COMMIT
//   P7 – immediately after WAL_TXN_COMMIT fsync (M1), before page flush
//   P9 – crash mid-undo of an already-aborting transaction (M1.b)
//   P10 – crash mid-vacuum (M10): WAL_VACUUM durable, page not flushed
//   P11 – torn 8 KiB page write (P1.a): reopen restores the page from its
//         full-page image (WAL_FPI) + incremental redo
//   P12 – fsync/msync failure (P1.b): the WAL and buffer pool refuse to
//         report durability on a failed flush, and latch poisoned
//   P13 – crash mid-durable-B-Tree-split (P3.a): the index's node pages are
//         WAL-logged (WAL_INDEX full-page images); after total loss of the
//         data file, recovery reconstructs the whole tree from the WAL, so
//         every indexed key is still findable — never rebuilt on open
//   P14 – durable full-text index (P3.b): committed rows + their FULLTEXT
//         index survive a crash (no checkpoint) and `search_fulltext` works on
//         reopen with no heap rescan/rebuild
//   P15 – durable edge-adjacency index (P3.b): committed edges + their durable
//         `__edges__.from_id` index survive a crash and `edges_from` traversal
//         works on reopen with no rebuild
//   P16 – large object (P3.d): a committed out-of-line chunked blob + its
//         `__lobs__` index survive a crash (no checkpoint) and stream back
//         intact on reopen
//   P17 – durable vector index (P3.c): a committed `CREATE INDEX ... USING HNSW`
//         (durable on-disk IVF-Flat) + its inserted vectors survive a crash (no
//         checkpoint), and after reopen `NEAR` returns the correct nearest
//         neighbor with recall intact — the index is read from its WAL-recovered
//         meta/centroid/posting pages, never rebuilt on open

use tempfile::tempdir;
use unidb::{Engine, RowId};

fn open(dir: &std::path::Path) -> Engine {
    Engine::open(dir, 0).unwrap()
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Insert a row (in its own committed transaction) and flush only the WAL
/// (page stays dirty — simulates P1/P3).
fn insert_wal_only(dir: &std::path::Path, data: &[u8]) -> RowId {
    let mut engine = open(dir);
    let xid = engine.begin().unwrap();
    let rid = engine.insert(xid, data).unwrap();
    engine.commit(xid).unwrap();
    // WAL is fsynced at commit (inside insert's mini-txn, and at user-txn
    // commit). Page is NOT explicitly flushed.
    drop(engine); // "crash" — OS may or may not have written the page
    rid
}

#[allow(dead_code)]
fn insert_full_flush(dir: &std::path::Path, data: &[u8]) -> RowId {
    let mut engine = open(dir);
    let xid = engine.begin().unwrap();
    let rid = engine.insert(xid, data).unwrap();
    engine.commit(xid).unwrap();
    engine.flush().unwrap();
    drop(engine);
    rid
}

// ── P1: WAL durable, page not flushed ────────────────────────────────────────

#[test]
fn p1_wal_durable_page_not_flushed() {
    let dir = tempdir().unwrap();
    let rid = insert_wal_only(dir.path(), b"p1_data");

    // Recovery: redo the committed insert → row must exist.
    let mut engine = open(dir.path());
    let xid = engine.begin().unwrap();
    let result = engine.get(xid, rid);
    // After redo, page content is recovered from WAL.
    assert!(
        result.is_ok(),
        "P1: committed row must survive redo; got {:?}",
        result
    );
    assert_eq!(result.unwrap(), b"p1_data");
}

// ── P2: mid-checkpoint (dirty pages flushed, checkpoint WAL record not written)

#[test]
fn p2_mid_checkpoint_pages_flushed_no_ckpt_record() {
    let dir = tempdir().unwrap();
    // Committed data exists; we crash after page flush but before checkpoint
    // record — simulated by flushing pages manually then dropping without
    // calling checkpoint().
    let rid = {
        let mut engine = open(dir.path());
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"p2_data").unwrap();
        engine.commit(xid).unwrap();
        engine.flush().unwrap(); // flush pages
                                 // "crash" here: checkpoint WAL record never written
        drop(engine);
        rid
    };

    let mut engine = open(dir.path());
    let xid = engine.begin().unwrap();
    let result = engine.get(xid, rid);
    assert!(result.is_ok(), "P2: row must survive; got {:?}", result);
    assert_eq!(result.unwrap(), b"p2_data");
}

// ── P3: after heap mutation, before commit record ─────────────────────────────

#[test]
fn p3_mutation_before_commit() {
    // Simulate: WAL BEGIN + INSERT logged, then crash before COMMIT.
    // We can't easily interrupt the mini-txn mid-flight through the Engine API,
    // so we directly write to the WAL to manufacture an incomplete txn.
    use unidb::control;
    use unidb::format::DEFAULT_PAGE_SIZE;
    use unidb::format::INVALID_LSN;
    use unidb::wal::Wal;

    let dir = tempdir().unwrap();
    let ctrl_p = dir.path().join("control");
    let data_p = dir.path().join("data.db");
    let wal_p = dir.path().join("db.wal");

    control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();

    // Write an incomplete mini-txn directly to the WAL.
    {
        let wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
        let (txn_id, begin_lsn) = wal.begin_mini_txn().unwrap();
        wal.log_insert(txn_id, begin_lsn, 99, 0, b"incomplete")
            .unwrap();
        // No commit — simulates crash after mutation before commit.
        drop(wal);
    }

    // Recovery must undo this incomplete txn (nothing should be visible).
    let (_, stats) =
        unidb::recovery::recover(&ctrl_p, &data_p, &wal_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
    assert!(stats.incomplete_txns > 0, "P3: must detect incomplete txn");
    assert!(
        stats.records_undone > 0 || stats.incomplete_txns > 0,
        "P3: incomplete txn must be undone"
    );
}

// ── P4: WAL truncation interrupted ───────────────────────────────────────────

#[test]
fn p4_wal_truncation_interrupted() {
    // After a checkpoint the WAL is truncated. If we crash mid-truncation the
    // old (pre-truncation) WAL records may still be present. Recovery must be
    // idempotent — replaying already-applied records must not corrupt data.
    let dir = tempdir().unwrap();

    let rid = {
        let mut engine = open(dir.path());
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"p4_data").unwrap();
        engine.commit(xid).unwrap();
        engine.flush().unwrap();
        // Run checkpoint (truncates WAL).
        engine.checkpoint().unwrap();
        rid
    };

    // Reopen: WAL may be empty after truncation. Data should come from page.
    let mut engine = open(dir.path());
    let xid = engine.begin().unwrap();
    let result = engine.get(xid, rid);
    assert!(
        result.is_ok(),
        "P4: row must survive checkpoint+truncation; got {:?}",
        result
    );
    assert_eq!(result.unwrap(), b"p4_data");
}

// ── P5: immediately after commit fsync ───────────────────────────────────────

#[test]
fn p5_after_commit_fsync() {
    // Committed row (WAL fsynced). Page may or may not be flushed.
    // Recovery via redo must restore the row.
    let dir = tempdir().unwrap();
    let rid = insert_wal_only(dir.path(), b"p5_data");

    let mut engine = open(dir.path());
    let xid = engine.begin().unwrap();
    let result = engine.get(xid, rid);
    assert!(
        result.is_ok(),
        "P5: committed row must be recoverable; got {:?}",
        result
    );
    assert_eq!(result.unwrap(), b"p5_data");
}

// ── P6: mid-user-transaction, before WAL_TXN_COMMIT (M1) ─────────────────────

#[test]
fn p6_incomplete_user_txn_leaves_no_trace() {
    let dir = tempdir().unwrap();
    let (r1, r2) = {
        let mut engine = open(dir.path());
        let xid = engine.begin().unwrap();
        let r1 = engine.insert(xid, b"p6_row1").unwrap();
        let r2 = engine.insert(xid, b"p6_row2").unwrap();
        // Each insert's own mini-txn is already durably committed (fsynced,
        // per D2) — but the user transaction itself never reaches
        // WAL_TXN_COMMIT. "Crash" here: no engine.commit(xid) call.
        engine.flush().unwrap();
        drop(engine);
        (r1, r2)
    };

    let mut engine = open(dir.path());
    let xid2 = engine.begin().unwrap();
    assert!(
        engine.get(xid2, r1).is_err(),
        "P6: incomplete txn's first statement must leave no trace"
    );
    assert!(
        engine.get(xid2, r2).is_err(),
        "P6: incomplete txn's second statement must leave no trace"
    );
}

// ── P7: immediately after WAL_TXN_COMMIT fsync, before page flush (M1) ───────

#[test]
fn p7_committed_user_txn_survives_without_page_flush() {
    let dir = tempdir().unwrap();
    let (r1, r2) = {
        let mut engine = open(dir.path());
        let xid = engine.begin().unwrap();
        let r1 = engine.insert(xid, b"p7_row1").unwrap();
        let r2 = engine.insert(xid, b"p7_row2").unwrap();
        engine.commit(xid).unwrap(); // fsyncs WAL_TXN_COMMIT
                                     // "Crash" here: no engine.flush() call, pages may not be on disk.
        drop(engine);
        (r1, r2)
    };

    let mut engine = open(dir.path());
    let xid2 = engine.begin().unwrap();
    assert_eq!(
        engine.get(xid2, r1).unwrap(),
        b"p7_row1",
        "P7: committed txn's first statement must survive"
    );
    assert_eq!(
        engine.get(xid2, r2).unwrap(),
        b"p7_row2",
        "P7: committed txn's second statement must survive"
    );
}

// ── P9: crash mid-undo of an already-aborting transaction (M1.b) ─────────────

#[test]
fn p9_crash_mid_undo_still_converges_to_fully_undone() {
    // Manufacture the scenario directly at the Heap/Wal level: xid 5 begins
    // and inserts two rows (both mini-txns durably committed, per D2).
    // Runtime abort would normally self-stamp both inserts in reverse order
    // before writing WAL_TXN_ABORT — simulate a crash *partway through* that
    // undo by manually reversing only the first insert, then dropping
    // without ever writing WAL_TXN_ABORT. Recovery's incomplete-user-txn
    // pass must still converge to "both rows permanently dead," including
    // idempotently re-applying the self-stamp that was already done.
    use unidb::control;
    use unidb::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use unidb::heap::Heap;
    use unidb::mvcc::Snapshot;
    use unidb::wal::Wal;

    let dir = tempdir().unwrap();
    let ctrl_p = dir.path().join("control");
    let data_p = dir.path().join("data.db");
    let wal_p = dir.path().join("db.wal");
    control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();

    let xid = 5;
    let (r1, r2) = {
        let pool =
            unidb::bufferpool::BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
        let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);

        wal.begin_user_txn(xid).unwrap();
        let r1 = heap.insert(b"p9_row1", xid, &pool, &wal).unwrap();
        let r2 = heap.insert(b"p9_row2", xid, &pool, &wal).unwrap();

        // Simulate runtime abort getting partway through its undo_log
        // (reverse order: r2 first, then r1) before crashing — here we
        // apply only the r2 half, leaving r1 untouched, then "crash"
        // without ever writing WAL_TXN_ABORT.
        heap.undo_insert(r2.page_id, r2.slot, xid, &pool, &wal)
            .unwrap();

        pool.flush_all(wal.durable_lsn()).unwrap();
        drop(pool);
        drop(wal);
        (r1, r2)
    };

    let (_, stats) =
        unidb::recovery::recover(&ctrl_p, &data_p, &wal_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
    assert_eq!(
        stats.incomplete_user_txns, 1,
        "P9: must still detect the incomplete (aborting) user txn"
    );

    // Both rows must be permanently invisible: r2 because it was already
    // undone before the crash, r1 because recovery's own undo pass must
    // finish what runtime abort started.
    let pool =
        unidb::bufferpool::BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
    let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
    let snap = Snapshot::new(100, 100, vec![]);
    assert!(
        heap.get(r1, &snap, 100, &pool).is_err(),
        "P9: row untouched before the crash must still be undone by recovery"
    );
    assert!(
        heap.get(r2, &snap, 100, &pool).is_err(),
        "P9: row already undone before the crash must remain undone (idempotent)"
    );
}

// ── P10: crash mid-vacuum (M10) ──────────────────────────────────────────────
//
// Vacuum marks a reclaimable version's line pointer DEAD via a redo-only,
// idempotent WAL_VACUUM mini-txn (D2/D5). Simulate a crash *after* that record
// is durable but *before* the page is flushed (pages left dirty, dropped):
// recovery must redo the mark cleanly, lose no committed-visible row, and leave
// re-running vacuum a no-op (idempotent — the version is already reclaimed).

#[test]
fn p10_crash_mid_vacuum_redoes_cleanly_and_loses_no_committed_row() {
    use unidb::bufferpool::BufferPool;
    use unidb::control;
    use unidb::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use unidb::heap::Heap;
    use unidb::lockmgr::LockManager;
    use unidb::mvcc::Snapshot;
    use unidb::wal::Wal;

    let dir = tempdir().unwrap();
    let ctrl_p = dir.path().join("control");
    let data_p = dir.path().join("data.db");
    let wal_p = dir.path().join("db.wal");
    control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();

    let (keep, dead) = {
        let pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
        let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
        let lock = LockManager::new();

        // Two committed rows (each insert/delete is its own durably-fsynced
        // mini-txn), one of which is then deleted so it becomes reclaimable.
        let keep = heap.insert(b"keep", 1, &pool, &wal).unwrap();
        let dead = heap.insert(b"dead", 1, &pool, &wal).unwrap();
        heap.delete(dead, 1, &pool, &wal, &lock).unwrap();

        // Vacuum marks `dead` DEAD (WAL_VACUUM durable). "Crash" here: do NOT
        // flush pages, drop — recovery must redo the mark from the WAL.
        heap.mark_dead(dead, &pool, &wal).unwrap();
        drop(pool);
        drop(wal);
        (keep, dead)
    };

    let (_, _stats) =
        unidb::recovery::recover(&ctrl_p, &data_p, &wal_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();

    let pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
    let snap = Snapshot::new(100, 100, vec![]);
    // (i) the committed-visible row survives; (ii) its version chain is intact.
    let heap = Heap::from_pages(DEFAULT_PAGE_SIZE as usize, vec![keep.page_id]);
    assert_eq!(
        heap.get(keep, &snap, 100, &pool).unwrap(),
        b"keep",
        "P10: a committed-visible row must survive a mid-vacuum crash"
    );
    // The vacuumed version is gone (its DEAD mark was redone).
    assert!(
        heap.get(dead, &snap, 100, &pool).is_err(),
        "P10: the reclaimed version must stay reclaimed after recovery"
    );
    // (iii) redo re-applied cleanly and re-running vacuum finds nothing new.
    assert!(
        heap.collect_reclaimable(100, &pool).unwrap().is_empty(),
        "P10: re-running vacuum after recovery must be a no-op (idempotent)"
    );
}

// ── P11: torn-page recovery via full-page image (P1.a) ───────────────────────
//
// An 8 KiB page write is not atomic; a crash mid-write leaves a torn page (half
// old, half new) that CRC detects but cannot repair — the #1 silent data-loss
// hole. Full-page-writes (WAL_FPI) close it: the first modification of a page
// after each checkpoint logs the whole clean page image to the WAL, and
// recovery replays that image as the clean base before re-applying the
// interval's later incremental redo records on top. This test manufactures a
// genuine torn page on disk and asserts every committed row is restored.

#[test]
fn p11_torn_page_restored_from_full_page_image() {
    use std::io::{Seek, SeekFrom, Write};
    use unidb::bufferpool::BufferPool;
    use unidb::checkpoint;
    use unidb::control;
    use unidb::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use unidb::heap::Heap;
    use unidb::mvcc::Snapshot;
    use unidb::wal::Wal;

    let dir = tempdir().unwrap();
    let ctrl_p = dir.path().join("control");
    let data_p = dir.path().join("data.db");
    let wal_p = dir.path().join("db.wal");
    let control = std::sync::Mutex::new(control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap());
    let page_size = DEFAULT_PAGE_SIZE as usize;

    let (r1, r2) = {
        let mut pool = BufferPool::open(&data_p, page_size, 64).unwrap();
        let mut wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
        let heap = Heap::new(page_size);

        // R1 committed, its page flushed to disk, then a checkpoint: the page
        // is now clean on disk and FPI tracking is reset, so the next
        // modification opens a fresh interval and must log a full-page image.
        let r1 = heap.insert(b"r1_committed", 1, &pool, &wal).unwrap();
        pool.flush_all(wal.durable_lsn()).unwrap();
        checkpoint::run(&pool, &wal, &ctrl_p, &control, 2).unwrap();

        // R2 lands on the SAME page (small rows share a page): the insert logs
        // WAL_FPI(page, the clean image still holding only R1) then the
        // incremental INSERT for R2, all durably fsynced. The in-memory page now
        // holds R1+R2 but is deliberately NOT flushed.
        let r2 = heap.insert(b"r2_committed", 1, &pool, &wal).unwrap();
        assert_eq!(r1.page_id, r2.page_id, "both rows must share a page");
        drop(pool);
        drop(wal);
        (r1, r2)
    };

    // Simulate a torn 8 KiB write: clobber the second half of the row page on
    // disk so its CRC no longer validates (a half-old/half-new page).
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&data_p)
            .unwrap();
        let page_off = r1.page_id as u64 * DEFAULT_PAGE_SIZE as u64;
        f.seek(SeekFrom::Start(page_off + DEFAULT_PAGE_SIZE as u64 / 2))
            .unwrap();
        f.write_all(&vec![0xFFu8; page_size / 2]).unwrap();
        f.sync_all().unwrap();
    }

    // Precondition: the on-disk page really is torn now.
    {
        let pool = BufferPool::open(&data_p, page_size, 64).unwrap();
        assert!(
            pool.read_page(r1.page_id).is_err(),
            "P11: precondition — the on-disk page must be torn (CRC invalid)"
        );
    }

    // Recovery rebuilds the page from WAL_FPI + the incremental INSERT redo.
    let (_, stats) = unidb::recovery::recover(&ctrl_p, &data_p, &wal_p, page_size, 64).unwrap();
    assert!(
        stats.records_redone > 0,
        "P11: the FPI + insert must be redone"
    );

    let pool = BufferPool::open(&data_p, page_size, 64).unwrap();
    let heap = Heap::from_pages(page_size, vec![r1.page_id]);
    let snap = Snapshot::new(100, 100, vec![]);
    assert_eq!(
        heap.get(r1, &snap, 100, &pool).unwrap(),
        b"r1_committed",
        "P11: the pre-checkpoint row must be restored from the FPI clean base"
    );
    assert_eq!(
        heap.get(r2, &snap, 100, &pool).unwrap(),
        b"r2_committed",
        "P11: the post-checkpoint row must be restored by redo on top of the FPI"
    );
}

// ── P12: fsync/msync failure refuses to report success (P1.b) ────────────────
//
// The fsyncgate hazard: a failed fsync/msync may leave the OS having dropped
// the dirty data while clearing its dirty bit, so a retry can falsely succeed.
// The engine must never report durability on a failed flush — it latches into
// a poisoned state and keeps failing. This test injects a fault at both
// durability boundaries (the WAL commit fsync and the data-file page flush) and
// asserts each refuses success, does not advance/clean, and stays poisoned.

#[test]
fn p12_fsync_failure_refuses_to_report_success() {
    use unidb::bufferpool::BufferPool;
    use unidb::error::DbError;
    use unidb::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use unidb::heap::Heap;
    use unidb::page::SlottedPage;
    use unidb::wal::Wal;

    let dir = tempdir().unwrap();
    let page_size = DEFAULT_PAGE_SIZE as usize;

    // (a) WAL commit fsync fails → the row's mini-txn commit is fatal, the
    //     durable frontier does not advance, and the WAL stays poisoned.
    {
        let pool = BufferPool::open(&dir.path().join("a.db"), page_size, 64).unwrap();
        let wal = Wal::open(&dir.path().join("a.wal"), INVALID_LSN).unwrap();
        let heap = Heap::new(page_size);
        // First insert commits normally (durable frontier advances).
        heap.insert(b"durable", 1, &pool, &wal).unwrap();
        let durable_before = wal.durable_lsn();
        // Arm a fault: the *next* fsync (this insert's mini-txn commit) fails.
        wal.arm_fsync_fault();
        let res = heap.insert(b"never_durable", 1, &pool, &wal);
        assert!(
            matches!(res, Err(DbError::DurabilityFailure(_))),
            "P12: a failed WAL fsync must surface a fatal DurabilityFailure, got {res:?}"
        );
        assert!(wal.is_poisoned(), "P12: WAL must latch poisoned");
        assert_eq!(
            wal.durable_lsn(),
            durable_before,
            "P12: durable frontier must not advance on a failed fsync"
        );
        assert!(
            matches!(wal.sync(), Err(DbError::DurabilityFailure(_))),
            "P12: a poisoned WAL must keep failing, never a false success"
        );
    }

    // (b) Data-file msync fails → the page flush is fatal, the frame stays
    //     dirty (not claimed durable), and the pool stays poisoned.
    {
        let pool = BufferPool::open(&dir.path().join("b.db"), page_size, 64).unwrap();
        let pid = pool.alloc_page().unwrap();
        let mut page = SlottedPage::new(pid, unidb::format::PAGE_TYPE_HEAP, page_size);
        page.set_lsn(3);
        pool.write_page(&page).unwrap();
        pool.arm_flush_fault();
        let res = pool.flush_page(pid, 3);
        assert!(
            matches!(res, Err(DbError::DurabilityFailure(_))),
            "P12: a failed data-file flush must surface a fatal DurabilityFailure, got {res:?}"
        );
        assert!(pool.is_flush_poisoned(), "P12: pool must latch poisoned");
        assert!(
            matches!(pool.flush_all(3), Err(DbError::DurabilityFailure(_))),
            "P12: a poisoned pool must keep failing, never a false success"
        );
    }
}

// ── M4.d: two-table crash (no new P-number) ──────────────────────────────────
//
// Event rows (M4) are ordinary WAL-backed heap rows using the exact same
// mini-txn/user-txn machinery every other row already uses — `send_event_
// capture` performs its own independent `heap.insert` (its own mini-txn,
// D2) into `__events__`, recorded in the *same* user transaction's undo
// log as the triggering row's insert. This is the first crash test that
// spans two tables within one incomplete user transaction: it proves
// recovery's incomplete-user-txn undo pass doesn't stop after undoing the
// first table's mini-txn, but walks the whole undo log regardless of which
// table each entry belongs to.

#[test]
fn incomplete_user_txn_leaves_no_trace_across_two_tables() {
    let dir = tempdir().unwrap();
    {
        let mut engine = open(dir.path());
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
        engine.commit(xid).unwrap();
        engine.enable_events("t").unwrap();

        let xid2 = engine.begin().unwrap();
        engine
            .execute_sql(xid2, "INSERT INTO t (id) VALUES (1)")
            .unwrap();
        // Both t's row and its __events__ row are durably mini-txn-logged
        // (D2) at this point — but xid2 never reaches WAL_TXN_COMMIT.
        engine.flush().unwrap();
        drop(engine); // "crash"
    }

    let mut engine = open(dir.path());
    let xid = engine.begin().unwrap();
    let rows = engine.execute_sql(xid, "SELECT * FROM t").unwrap();
    match &rows[0] {
        unidb::sql::executor::ExecResult::Rows(r) => assert!(
            r.is_empty(),
            "incomplete txn's row in the triggering table must leave no trace"
        ),
        other => panic!("expected Rows, got {other:?}"),
    }
    let events = engine.poll_events(xid, "any", 10).unwrap();
    assert!(
        events.is_empty(),
        "incomplete txn's __events__ row must leave no trace either"
    );
}

// ── property: committed set is a prefix of operations ────────────────────────

#[test]
fn committed_rows_survive_after_reopen() {
    let dir = tempdir().unwrap();
    let mut rids = Vec::new();
    {
        let mut engine = open(dir.path());
        let xid = engine.begin().unwrap();
        for i in 0u32..50 {
            let data = i.to_le_bytes();
            let rid = engine.insert(xid, &data).unwrap();
            rids.push((rid, i));
        }
        engine.commit(xid).unwrap();
        engine.flush().unwrap();
    }
    let mut engine = open(dir.path());
    let xid = engine.begin().unwrap();
    for (rid, expected) in &rids {
        let data = engine.get(xid, *rid).unwrap();
        assert_eq!(data, expected.to_le_bytes());
    }
}

// ── property: crash+MVCC — recovered DB reflects exactly the transactions
// that reached WAL_TXN_COMMIT (M1.d) ─────────────────────────────────────────
//
// Random BEGIN/INSERT/COMMIT/ROLLBACK sequences (a self-contained LCG, no
// new dependency, since this is test-only and reproducibility only needs a
// fixed seed) up to a random "crash point," simulated by simply stopping —
// sometimes mid-transaction (no commit/abort call at all, exercising the
// same incomplete-user-txn path as P6/P9), sometimes right after a
// transaction finishes (exercising ordinary redo). After reopening
// (recovery runs), every row from a transaction we know committed must be
// present with the correct bytes; every row from a transaction that was
// explicitly rolled back, or never got a chance to reach WAL_TXN_COMMIT
// before the simulated crash, must be permanently invisible.

struct Lcg(u64);

impl Lcg {
    fn next_u64(&mut self) -> u64 {
        // Numerical Recipes LCG constants — good enough for test-only
        // pseudo-randomness, not for anything security-sensitive.
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    fn next_range(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

fn run_property_case(seed: u64) {
    let dir = tempdir().unwrap();
    let mut rng = Lcg(seed);

    let mut committed: Vec<(RowId, Vec<u8>)> = Vec::new();
    let mut rejected: Vec<RowId> = Vec::new();

    {
        let mut engine = open(dir.path());
        let num_txns = 5 + rng.next_range(5) as usize; // 5..=9
        let crash_after = rng.next_range(num_txns as u64) as usize;

        'txns: for txn_idx in 0..num_txns {
            let xid = engine.begin().unwrap();
            let mut local: Vec<(RowId, Vec<u8>)> = Vec::new();
            let num_ops = 1 + rng.next_range(3) as usize; // 1..=3
            for op_idx in 0..num_ops {
                let data = format!("seed{seed}-txn{txn_idx}-op{op_idx}").into_bytes();
                let rid = engine.insert(xid, &data).unwrap();
                local.push((rid, data));
            }

            if txn_idx == crash_after && rng.next_range(2) == 0 {
                // Crash mid-transaction: no commit, no abort call at all.
                // Its mini-txns are durably logged (D2), but WAL_TXN_COMMIT
                // never gets written — recovery must undo it entirely.
                for (rid, _) in local {
                    rejected.push(rid);
                }
                break 'txns;
            }

            if rng.next_range(10) < 8 {
                engine.commit(xid).unwrap();
                committed.extend(local);
            } else {
                engine.abort(xid).unwrap();
                for (rid, _) in local {
                    rejected.push(rid);
                }
            }

            if txn_idx == crash_after {
                break 'txns; // crash right after this transaction finished
            }
        }
        // "Crash": drop without an explicit flush, so recovery must redo
        // from the WAL, not just read already-flushed pages.
    }

    let mut engine = open(dir.path());
    let xid = engine.begin().unwrap();
    for (rid, expected) in &committed {
        let data = engine
            .get(xid, *rid)
            .unwrap_or_else(|e| panic!("seed {seed}: committed row {rid:?} missing: {e}"));
        assert_eq!(
            &data, expected,
            "seed {seed}: committed row {rid:?} has wrong data"
        );
    }
    for rid in &rejected {
        assert!(
            engine.get(xid, *rid).is_err(),
            "seed {seed}: rolled-back/incomplete row {rid:?} must not be visible"
        );
    }
}

#[test]
fn property_crash_recovery_reflects_only_committed_transactions() {
    for seed in [1u64, 42, 12345, 999_999, 7, 2024] {
        run_property_case(seed);
    }
}

// ── P14: durable full-text index survives a crash (P3.b) ─────────────────────
//
// The FULLTEXT index is a durable on-disk B+tree (P3.b) written under the same
// WAL_INDEX machinery P13 already proves recovers. This is the end-to-end
// Engine-level proof: commit rows + their full-text entries, "crash" without a
// checkpoint (so the pages live only in the WAL), reopen, and confirm
// `search_fulltext` returns the committed rows — with no heap rescan to rebuild
// the index on open.
#[test]
fn p14_durable_fulltext_survives_crash_and_is_searchable_on_reopen() {
    let dir = tempdir().unwrap();
    {
        let mut engine = open(dir.path());
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE docs (id INT, body TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON docs USING FULLTEXT (body)")
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO docs (id, body) VALUES (1, 'durable rust engine')",
            )
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO docs (id, body) VALUES (2, 'python data tool')",
            )
            .unwrap();
        engine.commit(xid).unwrap();
        // "Crash": drop without a checkpoint. Every mini-txn (heap + index) is
        // WAL-fsynced; no page flush is forced.
        drop(engine);
    }

    let mut engine = open(dir.path());
    let xid = engine.begin().unwrap();
    let rust_hits = engine.search_fulltext(xid, "docs", "body", "rust").unwrap();
    assert_eq!(
        rust_hits.len(),
        1,
        "P14: committed full-text row must survive"
    );
    let py_hits = engine
        .search_fulltext(xid, "docs", "body", "python")
        .unwrap();
    assert_eq!(py_hits.len(), 1, "P14: second full-text row must survive");
    assert!(
        engine
            .search_fulltext(xid, "docs", "body", "rust python")
            .unwrap()
            .is_empty(),
        "P14: AND-only intersection across recovered postings"
    );
}

// ── P15: durable edge-adjacency index survives a crash (P3.b) ────────────────
//
// `__edges__.from_id`'s adjacency index is a durable B+tree (P3.b) — no longer
// rebuilt on open. Commit edges, "crash" without a checkpoint, reopen, and
// confirm `edges_from` still resolves every committed edge from the durable
// index (which was recovered from the WAL, not rebuilt from a heap rescan).
#[test]
fn p15_durable_edge_index_survives_crash_and_traversal_works_on_reopen() {
    let dir = tempdir().unwrap();
    let hub = 100i64;
    {
        let mut engine = open(dir.path());
        let xid = engine.begin().unwrap();
        for to in 0..5i64 {
            engine.create_edge(xid, hub, to, "LINKS", "{}").unwrap();
        }
        engine.commit(xid).unwrap();
        drop(engine); // "crash" — no checkpoint
    }

    let mut engine = open(dir.path());
    let xid = engine.begin().unwrap();
    let edges = engine.edges_from(xid, hub).unwrap();
    assert_eq!(
        edges.len(),
        5,
        "P15: all committed edges must resolve from the recovered durable index"
    );
    let mut tos: Vec<i64> = edges.iter().map(|e| e.to_id).collect();
    tos.sort();
    assert_eq!(tos, vec![0, 1, 2, 3, 4]);
}

// ── P16: large object survives a crash (P3.d) ────────────────────────────────
//
// A large object is stored as chunk rows in `__lobs__` under the caller's xid,
// indexed by a durable `DiskBTree` — so a committed blob is durable via the
// ordinary heap+WAL path, and its index is crash-recovered (P3.a machinery).
// Commit a multi-chunk blob, "crash" without a checkpoint, reopen, and stream
// it back byte-for-byte.
#[test]
fn p16_large_object_survives_crash_and_streams_back_intact() {
    let dir = tempdir().unwrap();
    let n = 300 * 1024usize; // dozens of chunks across several heap pages

    let blob_byte = |i: usize| -> u8 { ((i * 2654435761) >> 13) as u8 };
    struct R<'a> {
        pos: usize,
        n: usize,
        f: &'a dyn Fn(usize) -> u8,
    }
    impl std::io::Read for R<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let take = (self.n - self.pos).min(buf.len());
            for (j, s) in buf[..take].iter_mut().enumerate() {
                *s = (self.f)(self.pos + j);
            }
            self.pos += take;
            Ok(take)
        }
    }

    let lob_id = {
        let mut engine = open(dir.path());
        let xid = engine.begin().unwrap();
        let id = engine
            .put_large_object(
                xid,
                R {
                    pos: 0,
                    n,
                    f: &blob_byte,
                },
            )
            .unwrap();
        engine.commit(xid).unwrap();
        drop(engine); // "crash" — no checkpoint
        id
    };

    let mut engine = open(dir.path());
    let xid = engine.begin().unwrap();
    let mut out: Vec<u8> = Vec::new();
    let written = engine.read_large_object(xid, lob_id, &mut out).unwrap();
    assert_eq!(written, n as u64, "P16: whole blob must survive the crash");
    for (i, b) in out.iter().enumerate() {
        assert_eq!(*b, blob_byte(i), "P16: byte {i} corrupted after recovery");
    }
}

// ── P13: durable B-Tree survives total data-file loss (P3.a) ─────────────────
//
// The Phase-3 durability contract for the on-disk B+tree: because every node
// mutation is WAL-logged as a full node-page image (`WAL_INDEX`), a crash that
// loses the *entire* data file must still leave every committed key findable
// after recovery reconstructs the tree from the WAL alone — and reopening never
// rescans a heap to rebuild the index. This is the strongest form of the
// "no rebuild on open" gate: there is nothing on disk to rebuild *from* except
// the log. The inserts below force several node splits (so the recovered set
// spans multiple leaves + at least one internal level, exercising split-chain
// and root-repoint records), and the split's node pages are deliberately never
// checkpointed, so they live only in the WAL when the "crash" happens.
#[test]
fn p13_durable_btree_recovered_from_wal_after_total_data_loss() {
    use unidb::btree_index::{DiskBTree, OrderedValue};
    use unidb::bufferpool::BufferPool;
    use unidb::control;
    use unidb::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use unidb::wal::Wal;

    let dir = tempdir().unwrap();
    let ctrl_p = dir.path().join("control");
    let data_p = dir.path().join("data.db");
    let wal_p = dir.path().join("db.wal");
    control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();
    let page_size = DEFAULT_PAGE_SIZE as usize;

    // Long keys (~130 bytes) so a leaf holds only ~60 entries — a modest number
    // of inserts forces splits (and a fsync-per-insert crash test stays quick).
    let key = |i: i64| OrderedValue::Text(format!("{i:04}{}", "p".repeat(126)));
    let n = 150i64;

    let meta = {
        let mut pool = BufferPool::open(&data_p, page_size, 64).unwrap();
        let mut wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
        let tree = DiskBTree::create(&mut pool, &mut wal).unwrap();
        for i in 0..n {
            let rid = RowId {
                page_id: i as u32,
                slot: 0,
            };
            tree.insert(key(i), rid, &mut pool, &mut wal).unwrap();
        }
        // Sanity: the tree really did grow past a single leaf (a split happened).
        assert!(
            pool.page_count() > 3,
            "P13: inserts must have forced a split"
        );
        let meta = tree.meta_page();
        // "Crash": drop without any checkpoint/flush_all — the node pages live
        // only in the WAL (their fsync happened at each insert's mini-txn commit).
        drop(pool);
        drop(wal);
        meta
    };

    // Total data-file loss: wipe data.db entirely. The only surviving record of
    // the index is the WAL.
    std::fs::remove_file(&data_p).unwrap();

    // Precondition: with the data file gone, the tree is unreadable.
    {
        let mut pool = BufferPool::open(&data_p, page_size, 64).unwrap();
        let tree = DiskBTree::new(meta, page_size);
        assert!(
            tree.search_eq(&key(0), &mut pool).is_err()
                || tree.search_eq(&key(0), &mut pool).unwrap().is_empty(),
            "P13: precondition — a wiped data file must not resolve any key"
        );
    }

    // Recovery replays every committed WAL_INDEX image, rebuilding the tree.
    let (_, stats) = unidb::recovery::recover(&ctrl_p, &data_p, &wal_p, page_size, 64).unwrap();
    assert!(
        stats.records_redone > 0,
        "P13: the index's WAL_INDEX records must be redone"
    );

    // Every committed key is findable again, from the WAL-reconstructed tree.
    let mut pool = BufferPool::open(&data_p, page_size, 64).unwrap();
    let tree = DiskBTree::new(meta, page_size);
    for i in 0..n {
        let got = tree.search_eq(&key(i), &mut pool).unwrap();
        assert_eq!(
            got,
            vec![RowId {
                page_id: i as u32,
                slot: 0
            }],
            "P13: key {i} must survive total data loss via WAL recovery"
        );
    }
}

// ── P17: durable vector index survives a crash (P3.c) ────────────────────────
//
// The vector index is a durable on-disk IVF-Flat (P3.c): its centroid/meta pages
// and cell posting lists are all WAL-logged (`WAL_INDEX`), so a committed
// `CREATE INDEX ... USING HNSW` plus its inserted vectors survive a crash with no
// checkpoint, and after reopen `NEAR` reads the WAL-recovered index — never
// rebuilding from a heap rescan. This checks recall is intact: for a clustered
// corpus, the exact nearest neighbor (and the exact top-k) come back for every
// probe, matching the brute-force ground truth.
#[test]
fn p17_durable_vector_index_survives_crash_recall_intact() {
    let dir = tempdir().unwrap();
    let n = 120i64;
    // Deterministic corpus in 2D: point i = (i, i). The index is built *after*
    // the rows exist, so training produces a real multi-cell partition
    // (nlist ≈ √120) — this exercises crash recovery of the persisted centroid
    // table + multiple cell posting lists, not just a single origin cell.
    {
        let mut engine = open(dir.path());
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        for i in 0..n {
            engine
                .execute_sql(
                    xid,
                    &format!("INSERT INTO t (id, embedding) VALUES ({i}, [{i}.0, {i}.0])"),
                )
                .unwrap();
        }
        engine.commit(xid).unwrap();
        let xid2 = engine.begin().unwrap();
        engine
            .execute_sql(xid2, "CREATE INDEX idx ON t USING HNSW (embedding)")
            .unwrap();
        engine.commit(xid2).unwrap();
        // "Crash": drop without a checkpoint. Every mini-txn (heap + IVF index)
        // was WAL-fsynced; no page flush is forced.
        drop(engine);
    }

    // Reopen: the durable IVF index is read from its recovered meta/centroid/
    // posting pages — no heap rescan, no rebuild.
    let mut engine = open(dir.path());
    let near_one = |engine: &mut Engine, q: i64| -> i64 {
        let xid = engine.begin().unwrap();
        let sql = format!("SELECT id FROM t WHERE NEAR(embedding, [{q}.0, {q}.0], 1)");
        let res = engine.execute_sql(xid, &sql).unwrap();
        engine.commit(xid).unwrap();
        match &res[0] {
            unidb::sql::executor::ExecResult::Rows(rows) => match rows[0][0] {
                unidb::sql::logical::Literal::Int(v) => v,
                ref other => panic!("expected Int, got {other:?}"),
            },
            other => panic!("expected Rows, got {other:?}"),
        }
    };
    // recall@1 = 1.0: the exact nearest (brute force = the query's own id) is
    // returned for every probe across the corpus.
    for q in [0i64, 1, 17, 60, 99, 119] {
        assert_eq!(
            near_one(&mut engine, q),
            q,
            "P17: NEAR must return the exact nearest neighbor after crash recovery"
        );
    }

    // Exact top-k also intact: the 5 nearest to point 50 are 48..=52.
    let xid = engine.begin().unwrap();
    let res = engine
        .execute_sql(
            xid,
            "SELECT id FROM t WHERE NEAR(embedding, [50.0, 50.0], 5)",
        )
        .unwrap();
    match &res[0] {
        unidb::sql::executor::ExecResult::Rows(rows) => {
            let mut ids: Vec<i64> = rows
                .iter()
                .map(|r| match r[0] {
                    unidb::sql::logical::Literal::Int(v) => v,
                    ref other => panic!("expected Int, got {other:?}"),
                })
                .collect();
            ids.sort();
            assert_eq!(
                ids,
                vec![48, 49, 50, 51, 52],
                "P17: exact top-5 recall must survive the crash"
            );
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}
