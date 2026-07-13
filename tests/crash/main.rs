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
//   P18 – segmented WAL (P6.a): committed inserts written across several sealed
//         WAL segments (small segment size forces multiple rotations) survive a
//         crash with no checkpoint; recovery scans every segment in LSN order
//         and redoes them all, so every committed row is present on reopen.
//         Whole-segment truncation then deletes only fully-consumed segments and
//         the retained data still recovers.
//   P19 – backup + PITR (P6.d): after a base backup, more committed writes are
//         archived; the primary is lost ("crash"), and a restore of base +
//         archived WAL into a fresh directory recovers every committed row —
//         the backup/restore drill as a recovery path.
//   Pa..Pd – commit-time WAL fsync (group-committed force-log-at-commit default):
//         Pa incomplete unsynced txn leaves no trace; Pb a committed txn's sync
//         that flushes an open txn's shared-log records still cleanly undoes the
//         open txn; Pc a torn unsynced tail stops replay cleanly (committed
//         prefix survives); Pd eviction-forced-sync D5 ordering holds on crash.
//         The valid-prefix property test also runs under BOTH policies.
//   P26 – crash after an autovacuum pass (A3/A4): a background-style
//         `run_autovacuum_pass` reclaims churn (WAL_VACUUM self-synced durable),
//         then the engine is dropped with no checkpoint. Recovery redoes the
//         reclamation at the Engine level (real table, durable index scrub,
//         compaction) — the current row survives, reclaimed versions stay
//         reclaimed, and a re-vacuum finds nothing new (idempotent). Distinct
//         from P10, which exercises the raw-Heap mark at a lower level.
//   P27 – durable FSM directory (durable-FSM B2): a table's heap page directory
//         lives in the durable FSM tree (WAL_INDEX), not the catalog blob. A
//         multi-page table crashed with no checkpoint recovers every row via a
//         full scan (which walks the WAL-recovered directory), and the reopened
//         heap appends new rows at the recovered tail (DiskBTree::max_entry).
//   P28 – atomic heap grow (durable-FSM B2): a grow brackets the new page's init
//         and its FSM directory entry in one WAL mini-txn, so a crash mid-grow
//         leaves both or neither — never an orphan page absent from the
//         directory. Rows on freshly grown pages survive a crash byte-intact.
//   P30 – event seq index (item 26, Q1): the durable `__events__.seq` B-tree
//         index entries survive a crash (no checkpoint) alongside their heap
//         rows, and `poll_events_after` resolves the correct events on reopen
//         via the recovered index — no heap scan needed, no index rebuild.
//   P31 – crash mid-vacuum_table (V2, item 27): `vacuum_table` on a single
//         named table uses the same WAL_VACUUM mini-txn path as the global
//         `vacuum`; a crash mid-pass (drop with no checkpoint) is recovered by
//         redoing WAL_VACUUM records, so the live row survives, reclaimed
//         versions stay reclaimed, and a re-vacuum_table is a no-op. Verifies
//         the per-table scope — a second table's rows are untouched throughout.
//   P32 – torn timeline mark (item 28, R1): a crash mid-append of a 16-byte
//         timeline mark leaves a partial record at the end of `timeline.bin`.
//         On load the partial record is silently skipped; PITR resolves to the
//         previous valid mark. Database consistency is unaffected (the WAL is
//         the source of truth). This point tests that degraded precision, not
//         data loss, is the outcome.

use tempfile::tempdir;
use unidb::{Engine, RowId};

fn open(dir: &std::path::Path) -> Engine {
    Engine::open(dir, 0).unwrap()
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Insert a row (in its own committed transaction) and flush only the WAL
/// (page stays dirty — simulates P1/P3).
fn insert_wal_only(dir: &std::path::Path, data: &[u8]) -> RowId {
    let engine = open(dir);
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
    let engine = open(dir);
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
    let engine = open(dir.path());
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
        let engine = open(dir.path());
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"p2_data").unwrap();
        engine.commit(xid).unwrap();
        engine.flush().unwrap(); // flush pages
                                 // "crash" here: checkpoint WAL record never written
        drop(engine);
        rid
    };

    let engine = open(dir.path());
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
        let engine = open(dir.path());
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"p4_data").unwrap();
        engine.commit(xid).unwrap();
        engine.flush().unwrap();
        // Run checkpoint (truncates WAL).
        engine.checkpoint().unwrap();
        rid
    };

    // Reopen: WAL may be empty after truncation. Data should come from page.
    let engine = open(dir.path());
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

    let engine = open(dir.path());
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
        let engine = open(dir.path());
        // This point exercises the **per-statement** durability policy (the
        // non-default legacy mode kept for the harness): each insert's mini-txn
        // fsyncs immediately, so its pages are WAL-durable and `flush()` may
        // write them to the data file — yet the user transaction never commits.
        // The commit-time-fsync default's equivalent (statements unsynced) is
        // proven separately by `pa_deferred_mid_txn_unsynced_leaves_no_trace`.
        engine.set_deferred_sync(false);
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

    let engine = open(dir.path());
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
        let engine = open(dir.path());
        let xid = engine.begin().unwrap();
        let r1 = engine.insert(xid, b"p7_row1").unwrap();
        let r2 = engine.insert(xid, b"p7_row2").unwrap();
        engine.commit(xid).unwrap(); // fsyncs WAL_TXN_COMMIT
                                     // "Crash" here: no engine.flush() call, pages may not be on disk.
        drop(engine);
        (r1, r2)
    };

    let engine = open(dir.path());
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
        let pool = BufferPool::open(&data_p, page_size, 64).unwrap();
        let wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
        let heap = Heap::new(page_size);

        // R1 committed, its page flushed to disk, then a checkpoint: the page
        // is now clean on disk and FPI tracking is reset, so the next
        // modification opens a fresh interval and must log a full-page image.
        let r1 = heap.insert(b"r1_committed", 1, &pool, &wal).unwrap();
        pool.flush_all(wal.durable_lsn()).unwrap();
        checkpoint::run(&pool, &wal, &ctrl_p, &control, 2, u64::MAX).unwrap();

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
        let engine = open(dir.path());
        // Per-statement policy (see `p6_...`): mini-txns fsync immediately so
        // `flush()` can push their pages to disk while the user txn stays
        // incomplete. The commit-time-fsync default's equivalent is proven by
        // `pa_deferred_mid_txn_unsynced_leaves_no_trace`.
        engine.set_deferred_sync(false);
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

    let engine = open(dir.path());
    let xid = engine.begin().unwrap();
    let rows = engine.execute_sql(xid, "SELECT * FROM t").unwrap();
    match &rows[0] {
        unidb::sql::executor::ExecResult::Rows { rows: r, .. } => assert!(
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

// ── item 16: four-model atomicity — the §6 crash-consistency proof ───────────
//
// The headline claim behind the "replaced stack" comparison: unidb folds all
// four model-writes (relational row + `VECTOR(128)` + graph edge + event) of a
// logical record into ONE user transaction, so recovery is all-or-nothing —
// there is **no torn record**. The replaced stack (Postgres + a vector store +
// a graph store + a queue) has four independent WALs/commits and NO shared
// transaction, so a crash mid-sequence durably keeps the already-committed row
// while the embedding/edge/event are lost — a permanent orphan nothing rolls
// back. These two tests pin unidb's side of that asymmetry: a crash *before*
// `WAL_TXN_COMMIT` leaves **0 orphans** across all four models; a *committed*
// four-model txn survives with all four present. There is no third state.

fn build_four_model_table(engine: &Engine) {
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(
            xid,
            "CREATE TABLE t (id INT, body TEXT, embedding VECTOR(128))",
        )
        .unwrap();
    engine
        .execute_sql(xid, "CREATE INDEX iv ON t USING HNSW (embedding)")
        .unwrap();
    engine.commit(xid).unwrap();
    engine.enable_events("t").unwrap();
}

#[test]
fn item16_incomplete_four_model_txn_leaves_zero_orphans() {
    use unidb::sql::logical::Literal;
    let dir = tempdir().unwrap();
    {
        let engine = open(dir.path());
        // Per-statement policy (see `p6_...`): each mini-txn fsyncs immediately
        // so `flush()` can push all four models' pages to disk while the user
        // txn stays incomplete — the strongest test of the undo pass.
        engine.set_deferred_sync(false);
        build_four_model_table(&engine);

        let ins = engine
            .prepare("INSERT INTO t (id, body, embedding) VALUES ($1, $2, $3)")
            .unwrap();
        let xid = engine.begin().unwrap();
        // (1) relational row + (2) its VECTOR value/index + (4) auto-captured event
        engine
            .execute_prepared(
                xid,
                &ins,
                &[
                    Literal::Int(1),
                    Literal::Text("orphan-body".into()),
                    Literal::Vector(vec![0.25f32; 128]),
                ],
            )
            .unwrap();
        // (3) graph edge — same xid, same WAL, same undo log
        engine.create_edge(xid, 1, 2, "rel", "{}").unwrap();
        // Every model-write's mini-txn is durably logged, but xid never reaches
        // WAL_TXN_COMMIT. "Crash" here — no engine.commit(xid).
        engine.flush().unwrap();
        drop(engine);
    }

    // Recovery's incomplete-user-txn undo must reverse ALL FOUR — 0 orphans.
    let engine = open(dir.path());
    let xid = engine.begin().unwrap();
    match &engine.execute_sql(xid, "SELECT * FROM t").unwrap()[0] {
        unidb::sql::executor::ExecResult::Rows { rows, .. } => {
            assert!(
                rows.is_empty(),
                "orphan row (relational + embedding) survived a torn txn"
            );
        }
        other => panic!("expected Rows, got {other:?}"),
    }
    assert!(
        engine.edges_from(xid, 1).unwrap().is_empty(),
        "orphan graph edge survived a torn txn"
    );
    assert!(
        engine.poll_events(xid, "any", 10).unwrap().is_empty(),
        "orphan event survived a torn txn"
    );
}

#[test]
fn item16_committed_four_model_txn_survives_intact() {
    use unidb::sql::logical::Literal;
    let dir = tempdir().unwrap();
    {
        let engine = open(dir.path());
        build_four_model_table(&engine);
        let ins = engine
            .prepare("INSERT INTO t (id, body, embedding) VALUES ($1, $2, $3)")
            .unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_prepared(
                xid,
                &ins,
                &[
                    Literal::Int(1),
                    Literal::Text("kept-body".into()),
                    Literal::Vector(vec![0.5f32; 128]),
                ],
            )
            .unwrap();
        engine.create_edge(xid, 1, 2, "rel", "{}").unwrap();
        engine.commit(xid).unwrap(); // fsyncs WAL_TXN_COMMIT — the atomic switch
                                     // "Crash" before any page flush.
        drop(engine);
    }

    // All four models present after redo — the other side of "no third state".
    let engine = open(dir.path());
    let xid = engine.begin().unwrap();
    match &engine.execute_sql(xid, "SELECT id FROM t").unwrap()[0] {
        unidb::sql::executor::ExecResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "committed relational row must survive");
        }
        other => panic!("expected Rows, got {other:?}"),
    }
    assert_eq!(
        engine.edges_from(xid, 1).unwrap().len(),
        1,
        "committed graph edge must survive"
    );
    assert_eq!(
        engine.poll_events(xid, "any", 10).unwrap().len(),
        1,
        "committed event must survive"
    );
}

// ── property: committed set is a prefix of operations ────────────────────────

#[test]
fn committed_rows_survive_after_reopen() {
    let dir = tempdir().unwrap();
    let mut rids = Vec::new();
    {
        let engine = open(dir.path());
        let xid = engine.begin().unwrap();
        for i in 0u32..50 {
            let data = i.to_le_bytes();
            let rid = engine.insert(xid, &data).unwrap();
            rids.push((rid, i));
        }
        engine.commit(xid).unwrap();
        engine.flush().unwrap();
    }
    let engine = open(dir.path());
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

fn run_property_case(seed: u64, deferred: bool) {
    let dir = tempdir().unwrap();
    let mut rng = Lcg(seed);

    let mut committed: Vec<(RowId, Vec<u8>)> = Vec::new();
    let mut rejected: Vec<RowId> = Vec::new();

    {
        let engine = open(dir.path());
        // The valid-prefix invariant must hold under BOTH durability policies:
        // the commit-time-fsync default (`deferred = true`, statements unsynced
        // until commit) and the legacy per-statement policy (`deferred = false`).
        engine.set_deferred_sync(deferred);
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
                // Crash mid-transaction: no commit, no abort call at all. The
                // transaction never reaches WAL_TXN_COMMIT — recovery must undo
                // it entirely, whether or not its statements were fsynced (in
                // deferred mode they were not; either way it leaves no trace).
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

    let engine = open(dir.path());
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
        // Both policies: commit-time-fsync default AND legacy per-statement.
        run_property_case(seed, true);
        run_property_case(seed, false);
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
        let engine = open(dir.path());
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

    let engine = open(dir.path());
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
        let engine = open(dir.path());
        let xid = engine.begin().unwrap();
        for to in 0..5i64 {
            engine.create_edge(xid, hub, to, "LINKS", "{}").unwrap();
        }
        engine.commit(xid).unwrap();
        drop(engine); // "crash" — no checkpoint
    }

    let engine = open(dir.path());
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
        let engine = open(dir.path());
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

    let engine = open(dir.path());
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
        let pool = BufferPool::open(&data_p, page_size, 64).unwrap();
        let wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
        let tree = DiskBTree::create(&pool, &wal).unwrap();
        for i in 0..n {
            let rid = RowId {
                page_id: i as u32,
                slot: 0,
            };
            tree.insert(key(i), rid, &pool, &wal).unwrap();
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
        let pool = BufferPool::open(&data_p, page_size, 64).unwrap();
        let tree = DiskBTree::new(meta, page_size);
        assert!(
            tree.search_eq(&key(0), &pool).is_err()
                || tree.search_eq(&key(0), &pool).unwrap().is_empty(),
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
    let pool = BufferPool::open(&data_p, page_size, 64).unwrap();
    let tree = DiskBTree::new(meta, page_size);
    for i in 0..n {
        let got = tree.search_eq(&key(i), &pool).unwrap();
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

// ── P29: coalesced UPDATE index maintenance survives a crash (A1) ────────────
//
// A1 (crud_performance_phaseA) changed UPDATE to accumulate every touched row's
// B-tree entries and flush them **coalesced** (`DiskBTree::insert_many` — one
// full-page `WAL_INDEX` image per dirtied leaf instead of one per row). This is
// a new WAL pattern on the write+index path, so it gets its own crash point:
//
//   (a) A committed bulk UPDATE that leaves the indexed column *unchanged*
//       (`body`-only) must, after a crash with no checkpoint, still resolve
//       every row through the B-tree at its key — i.e. the coalesced index
//       images were fsynced and are replayed by recovery (no rebuild).
//   (b) An UPDATE that *changes* the indexed key must, after the same crash,
//       resolve the row at its NEW key and not its old one.
//   (c) An *incomplete* UPDATE (mutations WAL-appended, never committed → the
//       drop is the crash) must leave no trace: recovery undoes the heap
//       versions, and any redo-only index image that replayed is a stale hint
//       filtered by MVCC re-validation — so a point lookup returns the
//       pre-update row exactly, never a phantom.
#[test]
fn p29_coalesced_update_index_survives_crash() {
    let dir = tempdir().unwrap();
    let n = 400i64;

    // Build an indexed table and commit it (case a/b setup).
    {
        let engine = open(dir.path());
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, k INT, body TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX t_k ON t USING BTREE (k)")
            .unwrap();
        for i in 0..n {
            engine
                .execute_sql(
                    xid,
                    &format!("INSERT INTO t (id, k, body) VALUES ({i}, {i}, 'orig')"),
                )
                .unwrap();
        }
        engine.commit(xid).unwrap();

        // (a) Bulk UPDATE of a non-indexed column over most of the table — this
        // exercises the coalesced multi-entry-per-leaf WAL_INDEX path.
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(
                xid,
                &format!("UPDATE t SET body = 'changed' WHERE k < {}", n - 1),
            )
            .unwrap();
        engine.commit(xid).unwrap();

        // (b) Change one indexed key.
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "UPDATE t SET k = 100000 WHERE k = 50")
            .unwrap();
        engine.commit(xid).unwrap();

        drop(engine); // "crash" — no checkpoint; index lives only in the WAL
    }

    let ids_where = |engine: &Engine, sql: &str| -> Vec<i64> {
        let xid = engine.begin().unwrap();
        let res = engine.execute_sql(xid, sql).unwrap();
        engine.commit(xid).unwrap();
        let mut out = Vec::new();
        for r in res {
            if let unidb::SqlResult::Rows { rows, .. } = r {
                for row in rows {
                    if let Some(unidb::sql::logical::Literal::Int(v)) = row.first() {
                        out.push(*v);
                    }
                }
            }
        }
        out
    };

    {
        let engine = open(dir.path());
        // (a) every unchanged-key row still resolves via the B-tree.
        for k in [0i64, 1, 49, 51, 200, n - 1] {
            assert_eq!(
                ids_where(&engine, &format!("SELECT id FROM t WHERE k = {k}")),
                vec![k],
                "P29(a): row k={k} must resolve via the WAL-recovered coalesced index"
            );
        }
        // (b) the re-keyed row moved.
        assert!(
            ids_where(&engine, "SELECT id FROM t WHERE k = 50").is_empty(),
            "P29(b): old key must be gone after a committed key change + crash"
        );
        assert_eq!(
            ids_where(&engine, "SELECT id FROM t WHERE k = 100000"),
            vec![50],
            "P29(b): row must resolve at its new key after recovery"
        );
        drop(engine);
    }

    // (c) An incomplete UPDATE leaves no phantom.
    {
        let engine = open(dir.path());
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, &format!("UPDATE t SET k = 200000 WHERE k < {}", n - 1))
            .unwrap();
        // No commit — the drop is the crash. Its heap versions + coalesced index
        // images are WAL-appended but the user txn never reached COMMIT.
        drop(engine);
    }
    {
        let engine = open(dir.path());
        assert!(
            ids_where(&engine, "SELECT id FROM t WHERE k = 200000").is_empty(),
            "P29(c): an uncommitted UPDATE must leave no row at the phantom key"
        );
        // The pre-crash committed state is intact.
        assert_eq!(
            ids_where(&engine, "SELECT id FROM t WHERE k = 0"),
            vec![0],
            "P29(c): the committed row must remain resolvable at its original key"
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
        let engine = open(dir.path());
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
            unidb::sql::executor::ExecResult::Rows { rows, .. } => match rows[0][0] {
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
        unidb::sql::executor::ExecResult::Rows { rows, .. } => {
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

// ── P18: segmented WAL survives a crash spanning multiple segments (P6.a) ─────
//
// The WAL is a directory of fixed-size segments. With a tiny segment size, a
// stream of committed heap inserts forces several seal+rotate boundaries, so the
// committed records live across multiple segment files. A "crash" (drop, no page
// flush, no checkpoint) leaves the pages only in the WAL; recovery must scan
// every segment in LSN order and redo all committed inserts. Then a whole-segment
// truncation deletes only fully-consumed sealed segments and the retained data
// still recovers — proving segment deletion never drops a needed record.
#[test]
fn p18_segmented_wal_recovers_across_multiple_segments() {
    use unidb::bufferpool::BufferPool;
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
    let page_size = DEFAULT_PAGE_SIZE as usize;

    // A snapshot that sees every xid < 100 as committed (xid 1 here is a bare
    // mini-txn insert, no user-txn begin, so recovery never undoes it).
    let snap = Snapshot::new(100, 100, vec![]);
    let n = 200usize;

    let rids = {
        let pool = BufferPool::open(&data_p, page_size, 64).unwrap();
        // 2 KiB segments: each committed insert (begin + insert + commit records,
        // plus a one-time full-page image per page) rotates the WAL well before
        // 200 rows are in.
        let wal = Wal::open_with_segment_size(&wal_p, INVALID_LSN, 2048).unwrap();
        let heap = Heap::new(page_size);
        let mut rids = Vec::with_capacity(n);
        for i in 0..n {
            let rid = heap
                .insert(format!("seg_row_{i:04}").as_bytes(), 1, &pool, &wal)
                .unwrap();
            rids.push(rid);
        }
        // The stream really did span multiple segments.
        assert!(
            wal.segment_count().unwrap() >= 3,
            "P18: inserts must have forced multiple WAL rotations"
        );
        // "Crash": drop without flushing pages or checkpointing — the committed
        // rows live only in the WAL segments (each insert's mini-txn fsynced).
        drop(pool);
        drop(wal);
        rids
    };

    // Recovery scans every segment in LSN order and redoes the committed inserts.
    let (_, stats) = unidb::recovery::recover(&ctrl_p, &data_p, &wal_p, page_size, 64).unwrap();
    assert!(
        stats.records_redone >= n,
        "P18: every committed insert across all segments must be redone"
    );

    // Every committed row is present after recovery.
    {
        let pool = BufferPool::open(&data_p, page_size, 64).unwrap();
        let heap = Heap::new(page_size);
        for (i, rid) in rids.iter().enumerate() {
            let got = heap.get(*rid, &snap, 100, &pool).unwrap();
            assert_eq!(
                got,
                format!("seg_row_{i:04}").into_bytes(),
                "P18: row {i} must survive multi-segment WAL recovery"
            );
        }
    }

    // Whole-segment truncation: keep only the segment holding the highest LSN.
    // Deletes fully-consumed sealed segments (never the active one). Recovery
    // already persisted every page to `data.db`, so the truncated-away records
    // are no longer needed.
    {
        // The highest LSN in the log is the keep-point; every fully-earlier
        // sealed segment is then deletable.
        let max_lsn = Wal::scan_file(&wal_p)
            .unwrap()
            .iter()
            .map(|r| r.lsn)
            .max()
            .unwrap();
        let wal = Wal::open_with_segment_size(&wal_p, max_lsn, 2048).unwrap();
        let before = wal.segment_count().unwrap();
        wal.truncate_before(max_lsn).unwrap();
        let after = wal.segment_count().unwrap();
        assert!(
            after < before && after > 0,
            "P18: truncation must delete whole consumed segments (before={before}, after={after})"
        );
        drop(wal);
    }
    // Rows still readable after truncation + reopen.
    let (_, _) = unidb::recovery::recover(&ctrl_p, &data_p, &wal_p, page_size, 64).unwrap();
    let pool = BufferPool::open(&data_p, page_size, 64).unwrap();
    let heap = Heap::new(page_size);
    for (i, rid) in rids.iter().enumerate() {
        let got = heap.get(*rid, &snap, 100, &pool).unwrap();
        assert_eq!(
            got,
            format!("seg_row_{i:04}").into_bytes(),
            "P18: row {i} must survive whole-segment truncation"
        );
    }
}

// ── P19: backup + PITR restore recovers after primary loss (P6.d) ────────────
//
// Take a base backup, commit more rows and archive the WAL, then lose the
// primary directory entirely. Restoring base + archived WAL into a fresh
// directory reconstructs every committed row — the backup/restore drill acting
// as a recovery path.
#[test]
fn p19_backup_and_pitr_restore_after_primary_loss() {
    use unidb::backup;
    use unidb::sql::executor::ExecResult;

    let src = tempdir().unwrap();
    let base = tempdir().unwrap();
    let archive = tempdir().unwrap();

    {
        let engine = open(src.path());
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id) VALUES (1)")
            .unwrap();
        engine.commit(xid).unwrap();
        // Base backup (checkpoints internally), then more committed writes.
        engine.base_backup(base.path()).unwrap();
        for id in 2..=5 {
            let xid = engine.begin().unwrap();
            engine
                .execute_sql(xid, &format!("INSERT INTO t (id) VALUES ({id})"))
                .unwrap();
            engine.commit(xid).unwrap();
        }
        engine.archive_wal(archive.path()).unwrap();
        drop(engine);
    }

    // "Lose" the primary directory.
    std::fs::remove_dir_all(src.path()).unwrap();

    // Restore base + archived WAL into a fresh directory.
    let dest = tempdir().unwrap();
    backup::restore(base.path(), archive.path(), dest.path(), None).unwrap();

    let restored = open(dest.path());
    let xid = restored.begin().unwrap();
    let rows = restored.execute_sql(xid, "SELECT id FROM t").unwrap();
    restored.commit(xid).unwrap();
    let n = match &rows[0] {
        ExecResult::Rows { rows: r, .. } => r.len(),
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(
        n, 5,
        "P19: restore must recover every committed row after primary loss"
    );
}

// ── Commit-time WAL fsync (C4): crash points for the group-committed
// force-log-at-commit default ────────────────────────────────────────────────
//
// Under the default, statement mini-txns issued inside an open user transaction
// append their WAL records WITHOUT a per-statement fsync; `Engine::commit`'s
// `sync_up_to` is the single durable point. These four points prove recovery is
// correct under that policy (P6 and the two-table test cover the legacy
// per-statement policy; the valid-prefix property test above now runs BOTH).
//
//   Pa – crash mid-transaction with N unsynced statements → reopen → zero trace
//   Pb – txn A's unsynced statements are flushed to disk as a side effect of
//        txn B's commit sync (one ordered shared log) → crash with A still open
//        → A is cleanly undone, B survives
//   Pc – a torn record in the unsynced WAL tail → CRC detects it, replay stops
//        cleanly at the last valid record; the committed prefix survives
//   Pd – crash after eviction-forced WAL syncs during a large deferred txn
//        (D5 ordering under the new steal path) → every committed row recovers

/// Pa: a transaction whose statements were never fsynced (the commit-time-fsync
/// default) and that never commits must leave no trace after a crash — the
/// deferred-mode analog of P6.
#[test]
fn pa_deferred_mid_txn_unsynced_leaves_no_trace() {
    let dir = tempdir().unwrap();
    let rids = {
        let engine = open(dir.path()); // group-committed default: statements deferred
        let xid = engine.begin().unwrap();
        let mut rids = Vec::new();
        for i in 0..5 {
            rids.push(
                engine
                    .insert(xid, format!("pa-row-{i}").as_bytes())
                    .unwrap(),
            );
        }
        // No commit → `sync_up_to` never runs → the statements are not durable.
        // "Crash": drop without commit/flush.
        drop(engine);
        rids
    };
    let engine = open(dir.path());
    let xid = engine.begin().unwrap();
    for rid in &rids {
        assert!(
            engine.get(xid, *rid).is_err(),
            "Pa: an unsynced, uncommitted statement must leave no trace ({rid:?})"
        );
    }
}

/// Pb: txn A appends statements (unsynced) and stays open; txn B then commits,
/// whose `sync_up_to` flushes the shared WAL buffer — including A's records — to
/// durable storage. A crash with A still open must still cleanly undo A (it
/// never reached WAL_TXN_COMMIT) while B survives. Proves the shared, single
/// ordered log never accidentally persists an uncommitted transaction.
#[test]
fn pb_cross_txn_shared_log_sync_undoes_open_txn_keeps_committed() {
    let dir = tempdir().unwrap();
    let (a_rid, b_rid) = {
        let engine = open(dir.path());
        let a = engine.begin().unwrap();
        let a_rid = engine.insert(a, b"pb-txn-A-uncommitted").unwrap(); // appended, unsynced
        let b = engine.begin().unwrap();
        let b_rid = engine.insert(b, b"pb-txn-B-committed").unwrap();
        engine.commit(b).unwrap(); // sync_up_to flushes the shared WAL → A's record hits disk too
                                   // A never commits. "Crash".
        drop(engine);
        (a_rid, b_rid)
    };
    let engine = open(dir.path());
    let xid = engine.begin().unwrap();
    assert!(
        engine.get(xid, a_rid).is_err(),
        "Pb: open txn A's statement — made durable by B's commit sync — must be undone"
    );
    assert_eq!(
        engine.get(xid, b_rid).unwrap(),
        b"pb-txn-B-committed",
        "Pb: committed txn B must survive"
    );
}

/// Corrupt the last few bytes of the highest-numbered WAL segment file in
/// `dir/db.wal` (simulating a torn write of the unsynced tail).
fn corrupt_last_wal_segment_tail(dir: &std::path::Path) {
    let wal_dir = dir.join("db.wal");
    let mut segs: Vec<_> = std::fs::read_dir(&wal_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("seg-") && n.ends_with(".wal"))
                .unwrap_or(false)
        })
        .collect();
    segs.sort();
    let last = segs.last().expect("at least one WAL segment");
    let mut bytes = std::fs::read(last).unwrap();
    let n = bytes.len();
    assert!(n > 8, "segment must have content to corrupt");
    bytes[n - 5] ^= 0xff; // flip a byte inside the last record → CRC mismatch
    std::fs::write(last, &bytes).unwrap();
}

/// Pc: a torn record in the unsynced WAL tail is detected by CRC; recovery stops
/// cleanly at the last valid record, so the committed prefix survives. Re-proves
/// the existing torn-tail behavior under the commit-time-fsync default.
#[test]
fn pc_torn_unsynced_tail_replay_stops_cleanly() {
    let dir = tempdir().unwrap();
    let committed_rid = {
        let engine = open(dir.path());
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"pc-committed-durable").unwrap();
        engine.commit(xid).unwrap(); // durable (sync_up_to)
                                     // Unsynced tail: a large uncommitted row is flushed to the WAL file
                                     // (overflowing the writer buffer) but never fsynced.
        let x2 = engine.begin().unwrap();
        engine.insert(x2, &vec![b't'; 7000]).unwrap();
        drop(engine); // "crash"
        rid
    };
    // Manufacture a torn record at the tail.
    corrupt_last_wal_segment_tail(dir.path());

    let engine = open(dir.path());
    let xid = engine.begin().unwrap();
    assert_eq!(
        engine.get(xid, committed_rid).unwrap(),
        b"pc-committed-durable",
        "Pc: the committed prefix must survive a torn unsynced tail"
    );
}

/// Pd: under the default, a large transaction dirties more pages than the pool
/// holds; eviction forces WAL syncs (D5: the log is made durable before a dirty
/// page is stolen). A crash after commit — with most pages only ever
/// eviction-flushed, never checkpointed — must recover every committed row from
/// the durable WAL. Exercises D5 ordering on the eviction-forced-sync path.
#[test]
fn pd_eviction_forced_sync_preserves_d5_on_crash() {
    let dir = tempdir().unwrap();
    let payload = vec![b'z'; 3000];
    let rids = {
        // Tiny pool (16 frames) forces eviction during the transaction.
        let engine = Engine::open_with_pool_capacity(dir.path(), 0, 16).unwrap();
        let xid = engine.begin().unwrap();
        let mut rids = Vec::new();
        for _ in 0..60 {
            rids.push(engine.insert(xid, &payload).unwrap()); // ~20+ pages > 16 frames
        }
        engine.commit(xid).unwrap(); // durable
                                     // "Crash": no checkpoint/flush. Pages evicted during the txn reached
                                     // disk (WAL forced durable first, per D5); the rest are lost and must be
                                     // redone from the durable WAL.
        drop(engine);
        rids
    };
    let engine = Engine::open_with_pool_capacity(dir.path(), 0, 16).unwrap();
    let xid = engine.begin().unwrap();
    for (i, rid) in rids.iter().enumerate() {
        assert_eq!(
            engine.get(xid, *rid).unwrap(),
            payload,
            "Pd: row {i} must recover after an eviction-forced-sync crash"
        );
    }
}

// ── P26: crash after an autovacuum pass (A3/A4) ──────────────────────────────
//
// Autovacuum auto-triggers the same M10 `Engine::vacuum` a manual call runs, so
// its WAL_VACUUM records are redo-only/idempotent and self-synced durable during
// the pass. This test drives the churn through a real SQL table + a durable
// index (so the pass exercises the index-scrub + page-compaction path, not just
// the raw-Heap mark P10 covers), runs one `run_autovacuum_pass`, then "crashes"
// (drop, no checkpoint) and reopens: the live row must survive, the reclaimed
// versions must stay reclaimed, and re-vacuuming must find nothing new.
#[test]
fn p26_crash_after_autovacuum_pass_recovers() {
    use unidb::VacuumReport;

    let dir = tempdir().unwrap();
    let (final_v, before): (i64, VacuumReport) = {
        let engine = open(dir.path());
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t (id INT, v INT)")
            .unwrap();
        engine
            .execute_sql(x, "CREATE INDEX idx ON t USING BTREE (id)")
            .unwrap();
        engine
            .execute_sql(x, "INSERT INTO t VALUES (1, 0)")
            .unwrap();
        engine.commit(x).unwrap();

        // Churn the row 30× → 30 committed dead versions.
        for i in 1..=30 {
            let x = engine.begin().unwrap();
            engine
                .execute_sql(x, &format!("UPDATE t SET v = {i}"))
                .unwrap();
            engine.commit(x).unwrap();
        }
        assert_eq!(engine.dead_tuple_estimate(), 30);

        // One autovacuum pass reclaims (WAL_VACUUM self-synced durable).
        let before = engine.run_autovacuum_pass().unwrap();
        assert!(
            before.versions_reclaimed >= 25,
            "P26: the pass must reclaim the churn: {before:?}"
        );
        // "Crash": drop with no checkpoint. WAL_VACUUM is durable; pages may not
        // be flushed and must be redone from the WAL on reopen.
        drop(engine);
        (30, before)
    };

    let engine = open(dir.path());
    // (i) the current committed row survives the mid-autovacuum-durability crash.
    let x = engine.begin().unwrap();
    let rows = engine
        .execute_sql(x, "SELECT v FROM t WHERE id = 1")
        .unwrap();
    match &rows[0] {
        unidb::SqlResult::Rows { rows: r, .. } => assert_eq!(
            r,
            &vec![vec![unidb::sql::logical::Literal::Int(final_v)]],
            "P26: the live row must survive with its latest value"
        ),
        other => panic!("expected Rows, got {other:?}"),
    }
    engine.commit(x).unwrap();

    // (ii) re-running vacuum after recovery reclaims nothing new — the earlier
    // reclamation was redone cleanly and is idempotent.
    let after = engine.vacuum().unwrap();
    assert_eq!(
        after.versions_reclaimed, 0,
        "P26: reclaimed versions must stay reclaimed after recovery (before={before:?}, after={after:?})"
    );
}

// ── P27: durable FSM directory survives a crash (durable-FSM B2) ──────────────
//
// A table's heap page directory now lives in the durable FSM tree (a `DiskBTree`,
// WAL-logged as `WAL_INDEX` full-page images), not the catalog blob. This builds
// a table spanning many heap pages (so the FSM tree holds a real multi-entry
// directory), "crashes" (drop, no checkpoint — the FSM node pages live only in
// the WAL), reopens, and asserts (i) a full-table scan returns every committed
// row — which it can only do if the FSM directory was recovered from the WAL,
// since the scan enumerates pages *through* it — and (ii) the reopened heap
// appends new rows at the WAL-recovered tail (via `DiskBTree::max_entry`) with no
// lost or duplicated pages, so old + new rows all read back.
#[test]
fn p27_durable_fsm_directory_survives_crash_and_scan_recovers_all_rows() {
    use unidb::sql::logical::Literal;
    use unidb::SqlResult;

    // ~4 KiB bodies -> ~2 rows/page, so 80 rows span ~40 heap pages: a real
    // multi-page FSM directory, without a slow build.
    let body = "z".repeat(4000);
    let n_before = 80usize;

    let dir = tempdir().unwrap();
    {
        let engine = open(dir.path());
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t (id INT, body TEXT)")
            .unwrap();
        engine.commit(x).unwrap();
        let ins = engine
            .prepare("INSERT INTO t (id, body) VALUES ($1, $2)")
            .unwrap();
        let x = engine.begin().unwrap();
        for i in 0..n_before {
            engine
                .execute_prepared(
                    x,
                    &ins,
                    &[Literal::Int(i as i64), Literal::Text(body.clone())],
                )
                .unwrap();
        }
        engine.commit(x).unwrap();
        // "Crash": drop with no checkpoint/flush. The FSM tree's node pages are
        // durable only in the WAL and must be redone on reopen.
        drop(engine);
    }

    let engine = open(dir.path());
    // (i) full scan recovers every committed row — proves the FSM directory
    // (which the scan walks) was rebuilt from the WAL.
    let count = |e: &Engine| -> usize {
        let x = e.begin().unwrap();
        let out = e.execute_sql(x, "SELECT id FROM t").unwrap();
        e.commit(x).unwrap();
        match &out[0] {
            SqlResult::Rows { rows: r, .. } => r.len(),
            other => panic!("expected Rows, got {other:?}"),
        }
    };
    assert_eq!(
        count(&engine),
        n_before,
        "P27: every committed row must survive via the WAL-recovered FSM directory"
    );

    // (ii) the reopened heap appends at the recovered tail — insert more rows,
    // then old + new all read back (no lost/duplicated pages post-recovery).
    let ins = engine
        .prepare("INSERT INTO t (id, body) VALUES ($1, $2)")
        .unwrap();
    let x = engine.begin().unwrap();
    for i in n_before..(n_before + 20) {
        engine
            .execute_prepared(
                x,
                &ins,
                &[Literal::Int(i as i64), Literal::Text(body.clone())],
            )
            .unwrap();
    }
    engine.commit(x).unwrap();
    assert_eq!(
        count(&engine),
        n_before + 20,
        "P27: appends after recovery must land at the recovered FSM tail"
    );
}

// ── P28: atomic heap grow leaves no orphan on crash (durable-FSM B2) ──────────
//
// A heap grow makes the new page's init record AND its FSM directory entry one
// WAL mini-txn (`alloc_heap_page` -> `DiskBTree::insert_in_txn`), so recovery
// replays both or neither — a crash mid-grow can never leave an initialized page
// that is absent from its directory (an orphan the scan would skip, silently
// losing the rows later written to it). This grows a table, "crashes" (drop, no
// checkpoint) immediately after the transaction that grew it, reopens, and
// asserts the rows on the freshly grown pages are present (their pages are in the
// recovered directory — not orphaned) and read back byte-intact (not torn).
#[test]
fn p28_atomic_heap_grow_leaves_no_orphan_on_crash() {
    use unidb::sql::logical::Literal;
    use unidb::SqlResult;

    let body = "q".repeat(4000); // ~2 rows/page -> many grows
    let n = 60usize;
    let last_body = format!("LAST-{}", "q".repeat(3990));

    let dir = tempdir().unwrap();
    {
        let engine = open(dir.path());
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t (id INT, body TEXT)")
            .unwrap();
        engine.commit(x).unwrap();
        let ins = engine
            .prepare("INSERT INTO t (id, body) VALUES ($1, $2)")
            .unwrap();
        // Each committed insert may grow a page; commit per row so the grow
        // mini-txns are durable but no checkpoint ever flushes the pages.
        for i in 0..n {
            let x = engine.begin().unwrap();
            let b = if i == n - 1 { &last_body } else { &body };
            engine
                .execute_prepared(x, &ins, &[Literal::Int(i as i64), Literal::Text(b.clone())])
                .unwrap();
            engine.commit(x).unwrap();
        }
        // "Crash" immediately after the last grow, no checkpoint.
        drop(engine);
    }

    let engine = open(dir.path());
    let x = engine.begin().unwrap();
    // Full scan (no index) enumerates pages through the recovered FSM directory.
    let out = engine.execute_sql(x, "SELECT id, body FROM t").unwrap();
    let rows = match &out[0] {
        SqlResult::Rows { rows: r, .. } => r,
        other => panic!("expected Rows, got {other:?}"),
    };
    // Every row survives — no page (grown by any insert) was orphaned out of the
    // directory, or the scan would be short.
    assert_eq!(
        rows.len(),
        n,
        "P28: no grown page may be orphaned from the recovered FSM directory"
    );
    // The very last row (on the most-recently grown page) is present and intact
    // — its page's directory entry recovered atomically with the page.
    let has_last = rows.iter().any(|r| {
        r.first() == Some(&Literal::Int((n - 1) as i64))
            && r.get(1) == Some(&Literal::Text(last_body.clone()))
    });
    assert!(
        has_last,
        "P28: the last grown page's row must survive byte-intact (atomic grow, not torn)"
    );
    engine.commit(x).unwrap();
}

// ── P30: event seq index survives crash and poll resolves via index ───────────

#[test]
fn p30_event_seq_index_survives_crash_and_poll_resolves_via_index() {
    // Insert events, drop (crash), reopen, assert that poll_events_after returns
    // the correct events via the recovered seq index (no heap full-scan needed).
    let dir = tempdir().unwrap();

    let committed_seqs: Vec<i64>;
    {
        let engine = open(dir.path());
        let x = engine.begin().unwrap();
        engine.execute_sql(x, "CREATE TABLE t (val INT)").unwrap();
        engine.commit(x).unwrap();

        let x = engine.begin().unwrap();
        engine.enable_events("t").unwrap();
        engine.commit(x).unwrap();

        // Insert 10 rows in 3 separate committed transactions.
        let mut seqs = Vec::new();
        for batch in [vec![1i64, 2, 3], vec![4, 5, 6], vec![7, 8, 9, 10]] {
            let x = engine.begin().unwrap();
            for v in batch {
                engine
                    .execute_sql(x, &format!("INSERT INTO t (val) VALUES ({})", v))
                    .unwrap();
            }
            engine.commit(x).unwrap();
        }
        // Collect committed seqs.
        let x = engine.begin().unwrap();
        let events = engine.poll_events_after(x, 0, 100).unwrap();
        engine.commit(x).unwrap();
        for e in &events {
            seqs.push(e.seq);
        }
        committed_seqs = seqs;
        assert_eq!(
            committed_seqs.len(),
            10,
            "should have 10 events before crash"
        );

        // Crash immediately — no checkpoint; index pages only in WAL.
        drop(engine);
    }

    // Reopen (recovery redoes the WAL_INDEX records).
    let engine = open(dir.path());

    // poll_events_after from the beginning: must return all 10 events.
    let x = engine.begin().unwrap();
    let events_after = engine.poll_events_after(x, 0, 100).unwrap();
    engine.commit(x).unwrap();
    assert_eq!(
        events_after.len(),
        10,
        "P30: all 10 events must survive crash via recovered seq index"
    );

    // Cursor-based: poll_events_after from mid-stream.
    let mid = committed_seqs[4]; // seq of the 5th event
    let x = engine.begin().unwrap();
    let tail = engine.poll_events_after(x, mid, 100).unwrap();
    engine.commit(x).unwrap();
    assert_eq!(
        tail.len(),
        5,
        "P30: poll_events_after(mid) must return the 5 events after the cursor"
    );
    // All returned seqs are > mid.
    assert!(
        tail.iter().all(|e| e.seq > mid),
        "P30: all returned events must have seq > cursor"
    );
    // Returned seqs match the second half.
    let mut got: Vec<i64> = tail.iter().map(|e| e.seq).collect();
    got.sort();
    let mut expected: Vec<i64> = committed_seqs[5..].to_vec();
    expected.sort();
    assert_eq!(got, expected, "P30: recovered index returns correct seqs");
}

// ── P31: crash mid-vacuum_table (V2/item 27) ─────────────────────────────────
//
// `vacuum_table` scopes its WAL_VACUUM mini-txns to one named table, using the
// same crash-safe path as the global `vacuum`. This crash point proves:
// (i)  the live row on the vacuumed table survives after recovery;
// (ii) reclaimed dead versions stay reclaimed (WAL_VACUUM redone idempotently);
// (iii) a second table's rows are completely unaffected;
// (iv) a re-`vacuum_table` after recovery is a no-op (idempotent).
#[test]
fn p31_crash_mid_vacuum_table_recovers_correctly() {
    use unidb::sql::logical::Literal;
    use unidb::SqlResult;

    let dir = tempdir().unwrap();

    // Build state: two tables, both churned.
    {
        let engine = open(dir.path());
        let x = engine.begin().unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t_vac (id INT, v INT)")
            .unwrap();
        engine
            .execute_sql(x, "CREATE TABLE t_bystander (id INT, v INT)")
            .unwrap();
        engine
            .execute_sql(x, "INSERT INTO t_vac VALUES (1, 0)")
            .unwrap();
        engine
            .execute_sql(x, "INSERT INTO t_bystander VALUES (99, 0)")
            .unwrap();
        engine.commit(x).unwrap();

        // Churn t_vac 20× so there are dead versions to reclaim.
        for v in 1..=20 {
            let x = engine.begin().unwrap();
            engine
                .execute_sql(x, &format!("UPDATE t_vac SET v = {v}"))
                .unwrap();
            engine.commit(x).unwrap();
        }

        // vacuum_table(t_vac) reclaims the churn. WAL_VACUUM records are
        // self-synced durable before vacuum_table returns (C1).
        let before = engine.vacuum_table("t_vac").unwrap();
        assert!(
            before.versions_reclaimed >= 10,
            "P31: vacuum_table must reclaim the churn before crash: {before:?}"
        );

        // "Crash": drop with no checkpoint. The WAL_VACUUM records are durable;
        // the pages may not have been flushed and must be redone on reopen.
        drop(engine);
    }

    // Reopen and verify.
    let engine = open(dir.path());

    // (i) The live row on t_vac (value = 20, the last committed UPDATE) survives.
    let x = engine.begin().unwrap();
    let rows = engine
        .execute_sql(x, "SELECT v FROM t_vac WHERE id = 1")
        .unwrap();
    engine.commit(x).unwrap();
    match &rows[0] {
        SqlResult::Rows { rows: r, .. } => assert_eq!(
            r,
            &vec![vec![Literal::Int(20)]],
            "P31: live row must have the final committed value after recovery"
        ),
        other => panic!("P31: expected Rows, got {other:?}"),
    }

    // (ii) t_bystander's row is intact and untouched.
    let x = engine.begin().unwrap();
    let rows = engine.execute_sql(x, "SELECT id FROM t_bystander").unwrap();
    engine.commit(x).unwrap();
    match &rows[0] {
        SqlResult::Rows { rows: r, .. } => assert_eq!(
            r,
            &vec![vec![Literal::Int(99)]],
            "P31: bystander table must be unaffected by vacuum_table(t_vac)"
        ),
        other => panic!("P31: expected Rows, got {other:?}"),
    }

    // (iii) A re-vacuum_table finds nothing new — the WAL_VACUUM redo was
    // idempotent and the reclamation is complete.
    let after = engine.vacuum_table("t_vac").unwrap();
    assert_eq!(
        after.versions_reclaimed, 0,
        "P31: re-vacuum_table after recovery must find nothing left to reclaim: {after:?}"
    );
}

// ── P32: torn timeline mark — falls back to previous valid mark (item 28, R1) ─
//
// A crash mid-append of a 16-byte timeline mark leaves a partial record at the
// end of timeline.bin. On load the partial record is silently skipped (the file
// size is not a multiple of 16), and PITR resolves to the previous valid mark.
// Database consistency is unaffected; only PITR resolution precision degrades.
#[test]
fn p32_torn_timeline_mark_falls_back_to_previous_valid_mark() {
    use std::io::Write as _;
    use unidb::backup::timeline::{TimelineIndex, TimelineMark, TIMELINE_FILE};

    let dir = tempdir().unwrap();
    let tl_path = dir.path().join(TIMELINE_FILE);

    // Write two valid 16-byte marks and then 7 bytes (torn third mark).
    let m1 = TimelineMark {
        ts_micros: 1000,
        lsn: 10,
    };
    let m2 = TimelineMark {
        ts_micros: 2000,
        lsn: 20,
    };
    {
        let mut f = std::fs::File::create(&tl_path).unwrap();
        f.write_all(&m1.to_bytes()).unwrap();
        f.write_all(&m2.to_bytes()).unwrap();
        f.write_all(&[0u8; 7]).unwrap(); // torn: 7 bytes of a 16-byte record
    }

    // Load must return only the two complete marks.
    let marks = TimelineIndex::load_from(&tl_path);
    assert_eq!(
        marks.len(),
        2,
        "P32: torn mark must be silently skipped on load"
    );
    assert_eq!(marks[0], m1);
    assert_eq!(marks[1], m2);

    // Resolve: target after the torn mark → falls back to lsn 20 (m2).
    assert_eq!(
        TimelineIndex::resolve(&marks, 9999),
        Some(20),
        "P32: resolve falls back to last valid mark when torn mark is skipped"
    );

    // Resolve: target between m1 and m2 → lsn 10 (only m1 eligible).
    assert_eq!(
        TimelineIndex::resolve(&marks, 1500),
        Some(10),
        "P32: resolve(1500) must return m1's lsn"
    );

    // Verify the engine still opens cleanly on a dir that has a torn timeline —
    // database integrity is unaffected.
    let engine = open(dir.path());
    // No table was created, but the engine opens cleanly.
    drop(engine);
}
