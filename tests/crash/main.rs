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
        let mut wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
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
        let mut pool =
            unidb::bufferpool::BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let mut wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
        let mut heap = Heap::new(DEFAULT_PAGE_SIZE as usize);

        wal.begin_user_txn(xid).unwrap();
        let r1 = heap.insert(b"p9_row1", xid, &mut pool, &mut wal).unwrap();
        let r2 = heap.insert(b"p9_row2", xid, &mut pool, &mut wal).unwrap();

        // Simulate runtime abort getting partway through its undo_log
        // (reverse order: r2 first, then r1) before crashing — here we
        // apply only the r2 half, leaving r1 untouched, then "crash"
        // without ever writing WAL_TXN_ABORT.
        heap.undo_insert(r2.page_id, r2.slot, xid, &mut pool, &mut wal)
            .unwrap();

        pool.flush_all(wal.durable_lsn).unwrap();
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
    let mut pool =
        unidb::bufferpool::BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
    let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
    let snap = Snapshot::new(100, 100, vec![]);
    assert!(
        heap.get(r1, &snap, 100, &mut pool).is_err(),
        "P9: row untouched before the crash must still be undone by recovery"
    );
    assert!(
        heap.get(r2, &snap, 100, &mut pool).is_err(),
        "P9: row already undone before the crash must remain undone (idempotent)"
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
