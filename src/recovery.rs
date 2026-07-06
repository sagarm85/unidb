// Recovery (D1 — steal + no-force, ARIES-style):
//   1. Read control file → get checkpoint_lsn.
//   2. Redo all committed mini-transactions from checkpoint_lsn onward.
//   3. Undo any incomplete mini-transactions (no COMMIT record).
//
// Never panics on a bad page or corrupt WAL record — detects and reports (D1).
// Structured logging throughout (D13).

use std::{collections::HashSet, path::Path};

use crate::{
    bufferpool::BufferPool,
    control::{self, ControlData},
    error::{DbError, Result},
    format::{
        u64_from_le, Xid, INVALID_LSN, WAL_ABORT, WAL_BEGIN, WAL_CHECKPOINT, WAL_COMMIT,
        WAL_DELETE, WAL_INSERT, WAL_TXN_ABORT, WAL_TXN_BEGIN, WAL_TXN_COMMIT, WAL_UPDATE,
    },
    heap::decode_insert_redo,
    page::SlottedPage,
    wal::{Wal, WalRecord},
};

pub struct RecoveryStats {
    pub records_scanned: usize,
    pub records_redone: usize,
    pub records_undone: usize,
    pub incomplete_txns: usize,
    /// User transactions (M1) that began but never reached `WAL_TXN_COMMIT`
    /// — undone even though their individual statements' mini-txns may have
    /// each committed durably (D2's per-statement unit vs. M1's
    /// multi-statement unit are tracked independently; see txn.rs).
    pub incomplete_user_txns: usize,
}

pub fn recover(
    control_path: &Path,
    data_path: &Path,
    wal_path: &Path,
    page_size: usize,
    pool_capacity: usize,
) -> Result<(ControlData, RecoveryStats)> {
    tracing::info!("recovery: starting");

    let control = control::read(control_path)?;
    let ckpt_lsn = control.checkpoint_lsn;
    tracing::info!(checkpoint_lsn = ckpt_lsn, "recovery: control file read");

    let records = Wal::scan_file(wal_path)?;
    tracing::info!(count = records.len(), "recovery: WAL records scanned");

    // Only process records at or after the checkpoint LSN.
    let relevant: Vec<&WalRecord> = records
        .iter()
        .filter(|r| r.lsn >= ckpt_lsn || ckpt_lsn == INVALID_LSN)
        .collect();

    // ── analysis pass: find committed and incomplete mini-txns ───────────────
    let mut committed: HashSet<u64> = HashSet::new();
    let mut aborted: HashSet<u64> = HashSet::new();
    let mut started: HashSet<u64> = HashSet::new();

    for r in &relevant {
        match r.rec_type {
            WAL_BEGIN => {
                started.insert(r.mini_txn_id);
            }
            WAL_COMMIT => {
                committed.insert(r.mini_txn_id);
            }
            WAL_ABORT => {
                aborted.insert(r.mini_txn_id);
            }
            WAL_CHECKPOINT => {}
            _ => {}
        }
    }

    let incomplete: HashSet<u64> = started
        .difference(&committed)
        .filter(|id| !aborted.contains(id))
        .copied()
        .collect();

    tracing::info!(
        committed = committed.len(),
        incomplete = incomplete.len(),
        "recovery: analysis pass complete"
    );

    let mut pool = BufferPool::open(data_path, page_size, pool_capacity)?;
    let mut stats = RecoveryStats {
        records_scanned: relevant.len(),
        records_redone: 0,
        records_undone: 0,
        incomplete_txns: incomplete.len(),
        incomplete_user_txns: 0,
    };

    // ── redo pass: replay committed mutations ────────────────────────────────
    for r in &relevant {
        if r.rec_type == WAL_BEGIN
            || r.rec_type == WAL_COMMIT
            || r.rec_type == WAL_ABORT
            || r.rec_type == WAL_CHECKPOINT
        {
            continue;
        }
        if !committed.contains(&r.mini_txn_id) {
            continue;
        }

        match redo_record(r, &mut pool, page_size) {
            Ok(()) => stats.records_redone += 1,
            Err(e) => {
                tracing::warn!(lsn = r.lsn, error = %e, "recovery: redo skipped");
            }
        }
    }

    // ── undo pass: reverse incomplete mini-txns ──────────────────────────────
    // Collect undo targets in reverse LSN order.
    let mut undo_records: Vec<&WalRecord> = relevant
        .iter()
        .filter(|r| incomplete.contains(&r.mini_txn_id))
        .filter(|r| {
            r.rec_type == WAL_INSERT || r.rec_type == WAL_UPDATE || r.rec_type == WAL_DELETE
        })
        .copied()
        .collect();
    undo_records.sort_by_key(|r| std::cmp::Reverse(r.lsn));

    for r in undo_records {
        match undo_record(r, &mut pool, page_size) {
            Ok(()) => stats.records_undone += 1,
            Err(e) => {
                tracing::warn!(lsn = r.lsn, error = %e, "recovery: undo skipped");
            }
        }
    }

    // ── M1: undo incomplete user transactions ─────────────────────────────
    // A user transaction (xid) is a sequence of mini-txns tied together by
    // WAL_TXN_BEGIN/COMMIT/ABORT (txn.rs). Its individual statements may
    // each have already committed (and been redone above) — but if the
    // transaction as a whole never reached WAL_TXN_COMMIT, all of its
    // effects must be undone regardless. Ownership of a mutation is
    // recovered from the tuple bytes themselves (xmin for INSERT, the new
    // xmax value for an xmax-stamp UPDATE — see heap.rs), not a separate
    // xid field in the WAL wire format.
    let mut user_started: HashSet<Xid> = HashSet::new();
    let mut user_committed: HashSet<Xid> = HashSet::new();
    let mut user_aborted: HashSet<Xid> = HashSet::new();
    for r in &relevant {
        match r.rec_type {
            WAL_TXN_BEGIN => {
                user_started.insert(r.mini_txn_id);
            }
            WAL_TXN_COMMIT => {
                user_committed.insert(r.mini_txn_id);
            }
            WAL_TXN_ABORT => {
                user_aborted.insert(r.mini_txn_id);
            }
            _ => {}
        }
    }
    let incomplete_user_txns: HashSet<Xid> = user_started
        .difference(&user_committed)
        .filter(|xid| !user_aborted.contains(xid))
        .copied()
        .collect();
    stats.incomplete_user_txns = incomplete_user_txns.len();

    if !incomplete_user_txns.is_empty() {
        // Phase 1: revert xmax stamps this xid applied to pre-existing rows
        // (DELETE, or an UPDATE's old-version half) back to 0 (live).
        for r in relevant
            .iter()
            .filter(|r| r.rec_type == WAL_UPDATE && committed.contains(&r.mini_txn_id))
        {
            if let Ok(new_xmax) = decode_xmax(&r.redo) {
                if incomplete_user_txns.contains(&new_xmax) {
                    let mut page = fetch_or_create(&mut pool, r.page_id, page_size)?;
                    page.set_xmax(r.slot, 0)?;
                    pool.write_page(&page)?;
                    pool.unpin(r.page_id);
                    stats.records_undone += 1;
                }
            }
        }
        // Phase 2: force-self-stamp every row this xid inserted (INSERT, or
        // an UPDATE's new-version half) so it is permanently invisible.
        // Runs *after* phase 1 so that a row this xid both inserted and
        // later re-superseded within its own transaction ends up dead
        // (self-stamped) rather than incorrectly live (reverted to 0 by an
        // earlier phase-1 stamp targeting the same slot).
        for r in relevant.iter().filter(|r| {
            r.rec_type == WAL_INSERT && r.slot != u16::MAX && committed.contains(&r.mini_txn_id)
        }) {
            if let Ok((xmin, _, _)) = decode_insert_redo(&r.redo) {
                if incomplete_user_txns.contains(&xmin) {
                    let mut page = fetch_or_create(&mut pool, r.page_id, page_size)?;
                    page.set_xmax(r.slot, xmin)?;
                    pool.write_page(&page)?;
                    pool.unpin(r.page_id);
                    stats.records_undone += 1;
                }
            }
        }
    }

    // Flush all recovered pages to disk.
    pool.flush_all(INVALID_LSN)?;

    tracing::info!(
        redone = stats.records_redone,
        undone = stats.records_undone,
        incomplete_txns = stats.incomplete_txns,
        incomplete_user_txns = stats.incomplete_user_txns,
        "recovery: complete"
    );

    Ok((control, stats))
}

fn redo_record(r: &WalRecord, pool: &mut BufferPool, page_size: usize) -> Result<()> {
    match r.rec_type {
        WAL_INSERT => {
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            if r.slot == u16::MAX {
                // Page allocation record — nothing to redo on content.
                return Ok(());
            }
            // Only redo if current slot count ≤ slot (idempotent redo).
            if r.slot < page.slot_count_pub() {
                return Ok(()); // already applied
            }
            // M1: redo payload is [xmin:8][prev_page:4][prev_slot:2][payload]
            // (heap.rs::encode_insert_redo), not bare payload bytes.
            let (xmin, prev, payload) = decode_insert_redo(&r.redo)?;
            page.insert_versioned(payload, xmin, 0, prev)?;
            page.set_lsn(r.lsn);
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_UPDATE => {
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            if page.lsn() >= r.lsn {
                pool.unpin(r.page_id);
                return Ok(()); // already at or past this LSN
            }
            // M1: WAL_UPDATE is now only ever an xmax stamp (DELETE, or an
            // UPDATE's old-version half) — the redo payload IS the new xmax
            // value (8 bytes), not a full replacement payload.
            let xmax = decode_xmax(&r.redo)?;
            page.set_xmax(r.slot, xmax)?;
            page.set_lsn(r.lsn);
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_DELETE => {
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            if page.lsn() >= r.lsn {
                pool.unpin(r.page_id);
                return Ok(());
            }
            page.delete(r.slot)?;
            page.set_lsn(r.lsn);
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        _ => {}
    }
    Ok(())
}

fn undo_record(r: &WalRecord, pool: &mut BufferPool, page_size: usize) -> Result<()> {
    match r.rec_type {
        WAL_INSERT => {
            // Undo an insert = delete the slot.
            if r.slot == u16::MAX {
                return Ok(());
            }
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            match page.delete(r.slot) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_UPDATE => {
            // Undo an xmax stamp = restore the old xmax (stored in undo payload).
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            let old_xmax = decode_xmax(&r.undo)?;
            match page.set_xmax(r.slot, old_xmax) {
                Ok(()) | Err(DbError::TupleDeleted { .. }) => {}
                Err(e) => return Err(e),
            }
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        WAL_DELETE => {
            // Undo a delete = re-insert the old tuple at same slot position.
            // Simple approach: insert anew (slot may differ, but for M0 this is fine).
            let mut page = fetch_or_create(pool, r.page_id, page_size)?;
            page.insert(&r.undo)?;
            pool.write_page(&page)?;
            pool.unpin(r.page_id);
        }
        _ => {}
    }
    Ok(())
}

/// Decode an xmax-stamp WAL redo/undo payload (8 bytes LE): the value *is*
/// the xmax to apply, since a stamp's payload is nothing but the new xmax.
fn decode_xmax(buf: &[u8]) -> Result<u64> {
    let arr: [u8; 8] = buf.try_into().map_err(|_| DbError::WalCorrupt { lsn: 0 })?;
    Ok(u64_from_le(arr))
}

fn fetch_or_create(pool: &mut BufferPool, page_id: u32, page_size: usize) -> Result<SlottedPage> {
    use crate::format::PAGE_TYPE_HEAP;
    match pool.fetch_page(page_id) {
        Ok(p) => Ok(p),
        Err(DbError::PageNotFound { .. }) => {
            Ok(SlottedPage::new(page_id, PAGE_TYPE_HEAP, page_size))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bufferpool::BufferPool;
    use crate::control;
    use crate::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use crate::heap::Heap;
    use crate::wal::Wal;
    use tempfile::tempdir;

    fn paths(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        (dir.join("control"), dir.join("data.db"), dir.join("db.wal"))
    }

    #[test]
    fn clean_recovery_no_incomplete() {
        let dir = tempdir().unwrap();
        let (ctrl_p, data_p, wal_p) = paths(dir.path());

        control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();
        let mut pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let mut wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
        let mut heap = Heap::new(DEFAULT_PAGE_SIZE as usize);

        let rid = heap.insert(b"persistent", 1, &mut pool, &mut wal).unwrap();
        pool.flush_all(wal.durable_lsn).unwrap();
        drop(pool);
        drop(wal);

        let (_, stats) = recover(&ctrl_p, &data_p, &wal_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        assert_eq!(stats.incomplete_txns, 0);
        assert_eq!(stats.records_undone, 0);
        let _ = rid;
    }

    #[test]
    fn incomplete_user_txn_detected_and_undone() {
        let dir = tempdir().unwrap();
        let (ctrl_p, data_p, wal_p) = paths(dir.path());
        control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();

        let rid = {
            let mut pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
            let mut wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
            let mut heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
            let xid = 7;
            wal.begin_user_txn(xid).unwrap();
            let rid = heap
                .insert(b"never_committed", xid, &mut pool, &mut wal)
                .unwrap();
            // No WAL_TXN_COMMIT — simulates a crash mid-user-transaction.
            // The statement's own mini-txn is already durably committed
            // (D2), but the user transaction as a whole never finished.
            pool.flush_all(wal.durable_lsn).unwrap();
            drop(pool);
            drop(wal);
            rid
        };

        let (_, stats) = recover(&ctrl_p, &data_p, &wal_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        assert_eq!(
            stats.incomplete_user_txns, 1,
            "must detect the incomplete user txn"
        );
        assert!(stats.records_undone > 0, "must undo the orphaned insert");

        // After recovery, the row must be permanently invisible.
        let mut pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
        let snap = crate::mvcc::Snapshot::new(100, 100, vec![]);
        assert!(heap.get(rid, &snap, 100, &mut pool).is_err());
    }

    #[test]
    fn committed_user_txn_is_not_undone() {
        let dir = tempdir().unwrap();
        let (ctrl_p, data_p, wal_p) = paths(dir.path());
        control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();

        let rid = {
            let mut pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
            let mut wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
            let mut heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
            let xid = 7;
            let begin_lsn = wal.begin_user_txn(xid).unwrap();
            let rid = heap.insert(b"survives", xid, &mut pool, &mut wal).unwrap();
            wal.commit_user_txn(xid, begin_lsn).unwrap();
            pool.flush_all(wal.durable_lsn).unwrap();
            drop(pool);
            drop(wal);
            rid
        };

        let (_, stats) = recover(&ctrl_p, &data_p, &wal_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        assert_eq!(stats.incomplete_user_txns, 0);

        let mut pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
        let snap = crate::mvcc::Snapshot::new(100, 100, vec![]);
        assert_eq!(heap.get(rid, &snap, 100, &mut pool).unwrap(), b"survives");
    }

    #[test]
    fn recovery_redoes_committed_insert() {
        let dir = tempdir().unwrap();
        let (ctrl_p, data_p, wal_p) = paths(dir.path());

        control::create(&ctrl_p, DEFAULT_PAGE_SIZE).unwrap();
        {
            let mut pool = BufferPool::open(&data_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
            let mut wal = Wal::open(&wal_p, INVALID_LSN).unwrap();
            let mut heap = Heap::new(DEFAULT_PAGE_SIZE as usize);
            heap.insert(b"survived", 1, &mut pool, &mut wal).unwrap();
            // Simulate crash: do NOT flush page to disk.
            drop(wal);
            drop(pool);
        }

        // Recovery should redo the committed insert.
        let (_, stats) = recover(&ctrl_p, &data_p, &wal_p, DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        assert!(stats.records_redone > 0);
    }
}
