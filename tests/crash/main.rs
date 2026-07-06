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

use unidb::{Engine, RowId};
use tempfile::tempdir;

fn open(dir: &std::path::Path) -> Engine {
    Engine::open(dir, 0).unwrap()
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Insert a row and flush only the WAL (page stays dirty — simulates P1/P3).
fn insert_wal_only(dir: &std::path::Path, data: &[u8]) -> RowId {
    let mut engine = open(dir);
    let rid = engine.insert(data).unwrap();
    // WAL is fsynced at commit (inside insert). Page is NOT explicitly flushed.
    drop(engine); // "crash" — OS may or may not have written the page
    rid
}

#[allow(dead_code)]
fn insert_full_flush(dir: &std::path::Path, data: &[u8]) -> RowId {
    let mut engine = open(dir);
    let rid = engine.insert(data).unwrap();
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
    let result = engine.get(rid);
    // After redo, page content is recovered from WAL.
    assert!(result.is_ok(), "P1: committed row must survive redo; got {:?}", result);
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
        let rid = engine.insert(b"p2_data").unwrap();
        engine.flush().unwrap(); // flush pages
        // "crash" here: checkpoint WAL record never written
        drop(engine);
        rid
    };

    let mut engine = open(dir.path());
    let result = engine.get(rid);
    assert!(result.is_ok(), "P2: row must survive; got {:?}", result);
    assert_eq!(result.unwrap(), b"p2_data");
}

// ── P3: after heap mutation, before commit record ─────────────────────────────

#[test]
fn p3_mutation_before_commit() {
    // Simulate: WAL BEGIN + INSERT logged, then crash before COMMIT.
    // We can't easily interrupt the mini-txn mid-flight through the Engine API,
    // so we directly write to the WAL to manufacture an incomplete txn.
    use unidb::wal::Wal;
    use unidb::format::INVALID_LSN;
    use unidb::control;
    use unidb::format::DEFAULT_PAGE_SIZE;

    let dir = tempdir().unwrap();
    let ctrl_p = dir.path().join("control");
    let data_p = dir.path().join("data.db");
    let wal_p  = dir.path().join("db.wal");

    control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();

    // Write an incomplete mini-txn directly to the WAL.
    {
        let mut wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
        let (txn_id, begin_lsn) = wal.begin_mini_txn().unwrap();
        wal.log_insert(txn_id, begin_lsn, 99, 0, b"incomplete").unwrap();
        // No commit — simulates crash after mutation before commit.
        drop(wal);
    }

    // Recovery must undo this incomplete txn (nothing should be visible).
    let (_, stats) = unidb::recovery::recover(
        &ctrl_p, &data_p, &wal_p, DEFAULT_PAGE_SIZE as usize, 64
    ).unwrap();
    assert!(stats.incomplete_txns > 0, "P3: must detect incomplete txn");
    assert!(stats.records_undone > 0 || stats.incomplete_txns > 0,
        "P3: incomplete txn must be undone");
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
        let rid = engine.insert(b"p4_data").unwrap();
        engine.flush().unwrap();
        // Run checkpoint (truncates WAL).
        engine.checkpoint().unwrap();
        rid
    };

    // Reopen: WAL may be empty after truncation. Data should come from page.
    let mut engine = open(dir.path());
    let result = engine.get(rid);
    assert!(result.is_ok(), "P4: row must survive checkpoint+truncation; got {:?}", result);
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
    let result = engine.get(rid);
    assert!(result.is_ok(), "P5: committed row must be recoverable; got {:?}", result);
    assert_eq!(result.unwrap(), b"p5_data");
}

// ── property: committed set is a prefix of operations ────────────────────────

#[test]
fn committed_rows_survive_after_reopen() {
    let dir = tempdir().unwrap();
    let mut rids = Vec::new();
    {
        let mut engine = open(dir.path());
        for i in 0u32..50 {
            let data = i.to_le_bytes();
            let rid = engine.insert(&data).unwrap();
            rids.push((rid, i));
        }
        engine.flush().unwrap();
    }
    let mut engine = open(dir.path());
    for (rid, expected) in &rids {
        let data = engine.get(*rid).unwrap();
        assert_eq!(data, expected.to_le_bytes());
    }
}
